# Squisher

## What?
`squisher` is a program that takes a glTF or .glb file with PNG/JPG textures and produces a .glb file where the textures have been replaced with ASTC compressed KTX2 files, *explicitly breaking the glTF spec*: [source](https://www.khronos.org/registry/glTF/specs/2.0/glTF-2.0.html#_image_mimetype).

If you want your assets to use optimised textures but don't want to pay the runtime transcoding cost of [KHR_texture_basisu](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_texture_basisu/README.md), this tool is for you.

## Why?
Because [hotham](https://github.com/leetvr/hotham) works best with compressed textures, and doing this stuff by hand is time consuming. It takes *literal minutes*. And we all know that [a watched pot never boils](https://www.youtube.com/watch?v=eTFBxp0VW9M).

## How?
Install `squisher` with `cargo install`:

```bash
cargo install --git https://github.com/leetvr/squisher.git
```

Running `squisher` is easy:

```bash
squisher your_file.glb output.glb
```

Which will produce `output.glb`, containing ASTC compressed KTX2 textures.

You can also use uncompressed RGBA8 textures:

```bash
squisher --format rgba8 your_file.glb output.glb
```

## Requirements
To compile `squisher`, you need:
- [Rust](https://rustup.rs/) 1.67.1 or newer

To run `squisher` you must have the following available on your system PATH:
- [Khronos Texture Tools](https://github.khronos.org/KTX-Software/ktxtools) 4.1.0 or newer
