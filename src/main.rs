use anyhow::{anyhow, Result};
use clap::Parser;
use pipewire as pw;
use pw::spa::pod::Pod;
use pw::spa::utils::Id;
use pw::stream::{Stream, StreamFlags};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tracing::{debug, error, info, warn};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u32 = 1; // Mono for microphone

#[derive(Parser, Debug)]
#[command(name = "virtual-mic")]
#[command(about = "Create a virtual microphone and pipe audio files to it")]
struct Args {
    /// Audio file to play (supports mp3, wav, flac, ogg, aac)
    #[arg(short, long)]
    file: PathBuf,

    /// Loop the audio file
    #[arg(short, long, default_value = "false")]
    loop_audio: bool,

    /// Virtual microphone name
    #[arg(short, long, default_value = "VirtualMic")]
    name: String,

    /// Volume multiplier (0.0 - 2.0)
    #[arg(short, long, default_value = "1.0")]
    volume: f32,

    /// Also play audio through speakers (monitor mode)
    #[arg(short, long, default_value = "false")]
    monitor: bool,
}

struct AudioDecoder {
    path: PathBuf,
    loop_audio: bool,
    volume: f32,
    buffer: VecDeque<f32>,
    decoder: Option<Box<dyn symphonia::core::codecs::Decoder>>,
    format: Option<Box<dyn symphonia::core::formats::FormatReader>>,
    track_id: Option<u32>,
    source_sample_rate: Option<u32>,
}

impl AudioDecoder {
    fn new(path: PathBuf, loop_audio: bool, volume: f32) -> Self {
        Self {
            path,
            loop_audio,
            volume,
            buffer: VecDeque::with_capacity(SAMPLE_RATE as usize * 2),
            decoder: None,
            format: None,
            track_id: None,
            source_sample_rate: None,
        }
    }

    fn open(&mut self) -> Result<()> {
        let file = File::open(&self.path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = self.path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("No audio track found"))?;

        let track_id = track.id;
        let codec_params = &track.codec_params;

        self.source_sample_rate = codec_params.sample_rate;
        info!(
            "Audio: {} Hz, {} channels",
            self.source_sample_rate.unwrap_or(0),
            codec_params.channels.map(|c| c.count()).unwrap_or(0)
        );

        let decoder =
            symphonia::default::get_codecs().make(codec_params, &DecoderOptions::default())?;

        self.decoder = Some(decoder);
        self.format = Some(format);
        self.track_id = Some(track_id);

        Ok(())
    }

    fn decode_more(&mut self) -> Result<bool> {
        let format = self.format.as_mut().ok_or_else(|| anyhow!("Not opened"))?;
        let decoder = self.decoder.as_mut().ok_or_else(|| anyhow!("No decoder"))?;
        let track_id = self.track_id.ok_or_else(|| anyhow!("No track"))?;

        loop {
            match format.next_packet() {
                Ok(packet) => {
                    if packet.track_id() != track_id {
                        continue;
                    }

                    match decoder.decode(&packet) {
                        Ok(decoded) => {
                            let spec = *decoded.spec();
                            let duration = decoded.capacity() as u64;

                            let mut sample_buf = SampleBuffer::<f32>::new(duration, spec);
                            sample_buf.copy_interleaved_ref(decoded);

                            let samples = sample_buf.samples();
                            let source_channels = spec.channels.count();
                            let source_rate = self.source_sample_rate.unwrap_or(SAMPLE_RATE);

                            // Convert to mono and resample if needed
                            for i in (0..samples.len()).step_by(source_channels) {
                                // Mix to mono
                                let mono: f32 = (0..source_channels)
                                    .filter_map(|ch| samples.get(i + ch))
                                    .sum::<f32>()
                                    / source_channels as f32;

                                self.buffer.push_back(mono * self.volume);
                            }

                            // Simple linear resampling if rates don't match
                            if source_rate != SAMPLE_RATE {
                                let ratio = SAMPLE_RATE as f64 / source_rate as f64;
                                let old_len = self.buffer.len();
                                let new_len = (old_len as f64 * ratio) as usize;

                                let old_samples: Vec<f32> = self.buffer.drain(..).collect();
                                for i in 0..new_len {
                                    let src_idx = i as f64 / ratio;
                                    let idx0 = src_idx.floor() as usize;
                                    let idx1 = (idx0 + 1).min(old_samples.len() - 1);
                                    let frac = src_idx - idx0 as f64;

                                    let sample = if idx0 < old_samples.len() {
                                        old_samples[idx0] * (1.0 - frac as f32)
                                            + old_samples.get(idx1).unwrap_or(&0.0) * frac as f32
                                    } else {
                                        0.0
                                    };
                                    self.buffer.push_back(sample);
                                }
                            }

                            return Ok(true);
                        }
                        Err(e) => {
                            warn!("Decode error: {}", e);
                            continue;
                        }
                    }
                }
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // End of file
                    if self.loop_audio {
                        info!("Looping audio...");
                        self.open()?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                Err(e) => {
                    error!("Format error: {}", e);
                    return Err(e.into());
                }
            }
        }
    }

    fn fill_buffer(&mut self, output: &mut [f32]) -> Result<usize> {
        let mut filled = 0;

        while filled < output.len() {
            if self.buffer.is_empty() {
                if !self.decode_more()? {
                    // End of audio, fill rest with silence
                    for sample in &mut output[filled..] {
                        *sample = 0.0;
                    }
                    return Ok(output.len());
                }
            }

            while filled < output.len() && !self.buffer.is_empty() {
                output[filled] = self.buffer.pop_front().unwrap_or(0.0);
                filled += 1;
            }
        }

        Ok(filled)
    }
}

struct VirtualDevice {
    module_id: Option<u32>,
    remap_module_id: Option<u32>,
    loopback_module_id: Option<u32>,
    sink_name: String,
    source_name: String,
}

impl VirtualDevice {
    fn new(name: &str, monitor: bool) -> Result<Self> {
        let sink_name = format!("{}_sink", name);
        let source_name = name.to_string();

        // Step 1: Create a null-sink to receive audio
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={}", sink_name),
                &format!(
                    "sink_properties=device.description=\"{}_Output\"",
                    name
                ),
                &format!("rate={}", SAMPLE_RATE),
                &format!("channels={}", CHANNELS),
            ])
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to create null sink: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let module_id: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| anyhow!("Failed to parse module ID"))?;

        info!("Created null sink with module ID: {}", module_id);

        // Step 2: Create a remap-source that exposes the monitor as a proper microphone
        // This makes it appear as a real input device to browsers
        let monitor_name = format!("{}.monitor", sink_name);
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-remap-source",
                &format!("source_name={}", source_name),
                &format!("master={}", monitor_name),
                &format!(
                    "source_properties=device.description=\"{}\"",
                    name
                ),
            ])
            .output()?;

        if !output.status.success() {
            // Clean up the sink if remap fails
            let _ = Command::new("pactl")
                .args(["unload-module", &module_id.to_string()])
                .output();
            return Err(anyhow!(
                "Failed to create remap source: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let remap_module_id: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| anyhow!("Failed to parse remap module ID"))?;

        info!("Created remap source with module ID: {}", remap_module_id);

        // Step 3: Optionally create a loopback to play audio through speakers
        let loopback_module_id = if monitor {
            let monitor_name = format!("{}.monitor", sink_name);
            let output = Command::new("pactl")
                .args([
                    "load-module",
                    "module-loopback",
                    &format!("source={}", monitor_name),
                    "latency_msec=1",
                ])
                .output()?;

            if !output.status.success() {
                warn!(
                    "Failed to create loopback (audio won't play through speakers): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                None
            } else {
                let loopback_id: Option<u32> = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse()
                    .ok();
                if let Some(id) = loopback_id {
                    info!("Created loopback with module ID: {} (audio will play through speakers)", id);
                }
                loopback_id
            }
        } else {
            None
        };

        info!("Virtual microphone '{}' created - select it in your application", source_name);

        Ok(Self {
            module_id: Some(module_id),
            remap_module_id: Some(remap_module_id),
            loopback_module_id,
            sink_name,
            source_name,
        })
    }

    fn sink_name(&self) -> &str {
        &self.sink_name
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // Unload in reverse order: loopback, remap source, then sink
        if let Some(loopback_id) = self.loopback_module_id {
            info!("Cleaning up loopback (module {})", loopback_id);
            let _ = Command::new("pactl")
                .args(["unload-module", &loopback_id.to_string()])
                .output();
        }
        if let Some(remap_id) = self.remap_module_id {
            info!("Cleaning up remap source (module {})", remap_id);
            let _ = Command::new("pactl")
                .args(["unload-module", &remap_id.to_string()])
                .output();
        }
        if let Some(module_id) = self.module_id {
            info!("Cleaning up null sink (module {})", module_id);
            let _ = Command::new("pactl")
                .args(["unload-module", &module_id.to_string()])
                .output();
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    if !args.file.exists() {
        return Err(anyhow!("Audio file not found: {:?}", args.file));
    }

    // Create the virtual audio device (null sink with monitor)
    let virtual_device = VirtualDevice::new(&args.name, args.monitor)?;

    info!("Initializing PipeWire...");
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let decoder = Rc::new(RefCell::new(AudioDecoder::new(
        args.file.clone(),
        args.loop_audio,
        args.volume.clamp(0.0, 2.0),
    )));

    // Open the audio file
    decoder.borrow_mut().open()?;

    info!("Creating audio stream to virtual device...");

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: vec![
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaType,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_TYPE_audio)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaSubtype,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_SUBTYPE_raw)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_AUDIO_format,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_AUDIO_FORMAT_F32_LE)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_AUDIO_rate,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Int(SAMPLE_RATE as i32),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_AUDIO_channels,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Int(CHANNELS as i32),
                },
            ],
        }),
    )
    .map_err(|e| anyhow!("Failed to serialize format: {:?}", e))?
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("Invalid pod"))?];

    // Create stream that outputs to our null sink
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => format!("{}_player", args.name),
        *pw::keys::NODE_DESCRIPTION => format!("{} Audio Player", args.name),
        // Target our null sink
        "node.target" => virtual_device.sink_name(),
    };

    let stream = Stream::new(&core, &format!("{}_player", args.name), props)?;

    let decoder_clone = decoder.clone();
    let mainloop_weak = mainloop.downgrade();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            info!("Stream state: {:?} -> {:?}", old, new);
        })
        .process(move |stream, _| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if let Some(data) = datas.first_mut() {
                    let stride = std::mem::size_of::<f32>() * CHANNELS as usize;

                    let filled = if let Some(slice) = data.data() {
                        let samples: &mut [f32] = unsafe {
                            std::slice::from_raw_parts_mut(
                                slice.as_mut_ptr() as *mut f32,
                                slice.len() / std::mem::size_of::<f32>(),
                            )
                        };

                        let mut dec = decoder_clone.borrow_mut();
                        match dec.fill_buffer(samples) {
                            Ok(filled) => {
                                debug!("Filled {} samples", filled);
                                Some(filled)
                            }
                            Err(e) => {
                                error!("Failed to fill buffer: {}", e);
                                if let Some(ml) = mainloop_weak.upgrade() {
                                    ml.quit();
                                }
                                None
                            }
                        }
                    } else {
                        None
                    };

                    if let Some(filled) = filled {
                        let chunk = data.chunk_mut();
                        *chunk.size_mut() = (filled * std::mem::size_of::<f32>()) as u32;
                        *chunk.stride_mut() = stride as i32;
                        *chunk.offset_mut() = 0;
                    }
                }
            }
        })
        .register()?;

    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    info!("Virtual microphone '{}' is now active!", args.name);
    info!("Select '{}' as your microphone in applications", args.name);
    info!("Playing: {:?}", args.file);
    info!("Press Ctrl+C to stop");

    // Handle Ctrl+C
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        info!("\nShutting down...");
        running_clone.store(false, Ordering::SeqCst);
    })
    .ok();

    // Keep virtual_device alive until shutdown
    let _virtual_device = virtual_device;

    let timer = mainloop.loop_().add_timer({
        move |_| {
            if !running.load(Ordering::SeqCst) {
                std::process::exit(0);
            }
        }
    });
    timer.update_timer(
        Some(std::time::Duration::from_millis(100)),
        Some(std::time::Duration::from_millis(100)),
    );

    mainloop.run();

    info!("Goodbye!");
    Ok(())
}
