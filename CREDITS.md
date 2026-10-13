# Credits

Basalt builds on the work of others. Each component below keeps its own license.

## Alacritty

[Alacritty](https://github.com/alacritty/alacritty), Apache License 2.0.

- `vendor/alacritty_terminal` is Alacritty's terminal crate (0.26.0), changed for
  Basalt's ConPTY and process handling. The changes are listed in `vendor/CHANGES.md`;
  the license is in `vendor/alacritty_terminal/LICENSE-APACHE`.
- The GPU terminal grid in `app/src/rendering/gpu` follows Alacritty's renderer: the
  row-packed glyph atlas, the glyph cache and subpixel blending. The box drawing, block
  and powerline glyphs in `drawn_glyphs.rs` are adapted from Alacritty's built-in font.
- Glyphs are rasterized by Alacritty's [crossfont](https://github.com/alacritty/crossfont)
  0.8.1 (Apache-2.0). `patches/crossfont` is that crate with one change, marked in
  `src/directwrite/mod.rs`: fonts can be added from memory and are found by family
  name ahead of installed ones. Its license is in `patches/crossfont/LICENSE`.

## JetBrains Mono and Nerd Fonts

The terminal and interface font is [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono)
as patched by [Nerd Fonts](https://github.com/ryanoasis/nerd-fonts) ("JetBrainsMono
Nerd Font Mono", v3.3.0), SIL Open Font License 1.1. The Medium, Medium Italic, Bold
and Bold Italic faces are in `app/assets/fonts` with the license (`OFL.txt`), and are
embedded in `basalt.exe`.

## ConPTY

Microsoft's [ConPTY](https://github.com/microsoft/terminal) (`conpty.dll` and
`OpenConsole.exe`, package `Microsoft.Windows.Console.ConPTY` 1.24.260710001), MIT
License. The unmodified x64 binaries are in `runtime/windows/conpty` with their
license, and are bundled in `basalt.exe`, which writes them with the license to its
data folder.

## Slint

The interface is built with [Slint](https://slint.dev), used under the Slint
Royalty-free Desktop, Mobile, and Web Applications License 2.0.
`patches/i-slint-renderer-skia` is Slint's Skia renderer 1.13.1 with two changes:
the WGPU surface waits for Skia before resizing its swap chain, and
`register_font_from_memory` is public so the embedded font reaches the interface.
Its licenses are in `patches/i-slint-renderer-skia/LICENSES`.

## herdr

The agent screen rules in `app/src/runtime/agent_rules` are taken unchanged from
[herdr](https://github.com/herdrdev/herdr) (`distribution/agent-detection`), Apache
License 2.0 (`app/src/runtime/agent_rules/LICENSE-APACHE`).

## node-cross-spawn

Batch-file argument escaping in `core/src/system/process/command.rs` is adapted from
[node-cross-spawn](https://github.com/moxystudio/node-cross-spawn), MIT License
(`vendor/cmd-escaping/LICENSE-MIT`).

## CLI logos

The logos in `app/assets/cli` belong to their respective owners and are used only to
identify each tool. Their sources are listed in `app/assets/cli/sources.json`; icon
licenses are in `app/assets/cli/licenses`.
