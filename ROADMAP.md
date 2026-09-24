# Glint roadmap

## Later

- **JPEG decoder on Vulkan compute.** Entropy decoding with speculative parallel Huffman
  (Weißenberger & Schmidt, 2018), then IDCT and colour conversion in compute shaders, straight
  into the texture. Expected ~5–10 ms for 24 MP instead of ~60 ms in WIC. Baseline JPEG first;
  progressive JPEG is a separate, harder step. Falls back to WIC for anything unsupported.
