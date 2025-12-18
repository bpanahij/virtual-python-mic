# virtual-mic

A Linux CLI tool that creates a virtual microphone and pipes audio files through it. Applications like browsers, video conferencing software, and voice chat will see this as a real microphone input.

## Features

- Creates a virtual microphone visible to all applications
- Supports multiple audio formats: MP3, WAV, FLAC, OGG, AAC
- Audio looping for continuous playback
- Adjustable volume (0.0 - 2.0)
- Optional monitor mode to hear audio through speakers
- Automatic cleanup on exit

## Requirements

- Linux with PipeWire audio server
- PulseAudio compatibility layer (`pactl` command)
- Rust toolchain (for building)

## Installation

```bash
cargo build --release
```

The binary will be at `target/release/virtual-mic`.

## Usage

```bash
# Basic usage - play an audio file as microphone input
virtual-mic -f audio.mp3

# Loop the audio continuously
virtual-mic -f audio.mp3 -l

# Set custom volume (0.0 - 2.0)
virtual-mic -f audio.mp3 -v 0.5

# Custom microphone name
virtual-mic -f audio.mp3 -n "MyMicrophone"

# Monitor mode - also hear audio through speakers
virtual-mic -f audio.mp3 -m
```

### Options

| Flag | Long | Description | Default |
|------|------|-------------|---------|
| `-f` | `--file` | Audio file to play (required) | - |
| `-l` | `--loop-audio` | Loop the audio file | `false` |
| `-n` | `--name` | Virtual microphone name | `VirtualMic` |
| `-v` | `--volume` | Volume multiplier (0.0 - 2.0) | `1.0` |
| `-m` | `--monitor` | Play audio through speakers too | `false` |

## How It Works

1. Creates a PulseAudio null-sink to receive audio
2. Sets up a remap-source exposing the sink's monitor as a microphone
3. Uses PipeWire to stream decoded audio to the null-sink
4. Applications see the remap-source as a standard microphone input

## License

MIT
