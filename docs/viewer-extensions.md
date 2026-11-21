# Viewer extensions on macOS

Right-click a local file and choose **Preview with an extension…**. Each compatible viewer shows its file access before **Run viewer**. PawCommander includes a line counter for TXT, Markdown, log and CSV files. It counts newline-delimited lines in the preview, including an unterminated final line.

Use **Settings → Viewer extensions… → Add extension…** to install a package. The same menu lists installed viewers and lets you remove them. Installing a package validates its module and registers it. A viewer runs when you select it for a file.

## Package format

API version 1 packages are UTF-8 JSON files. Use [the packaged line counter](../assets/extensions/line-count.pawextension.json) and [its WAT source](../assets/extensions/line-count.wat) as a starting point.

| Field | Meaning |
| --- | --- |
| `api_version` | The integer `1`. |
| `id` | A unique identifier of up to 80 ASCII lowercase letters, digits, hyphens or dots. PawCommander reserves the `pawcommander.` prefix. |
| `name` | The viewer name, up to 120 UTF-8 bytes. |
| `extensions` | One to sixteen file suffixes, without dots, using lowercase ASCII letters or digits. Each is at most sixteen characters. Matching ignores ASCII case. |
| `permissions` | `[]` for no file access or `["read_selected_file"]` to receive up to the first 32 KiB of the selected file in an explicit run. |
| `module` | Standard Base64 encoding of the binary WebAssembly module. The module can occupy up to 64 KiB and the entire package up to 128 KiB. |

Compile edited WAT to binary WebAssembly with a tool such as `wat2wasm`. Encode the binary into `module` and choose a new `id` for the package.

## Module interface

A version 1 module exports 32-bit linear memory as `memory` and a function `paw_preview(input_length: i32) -> i64`. PawCommander creates a fresh instance for each run, writes the input at memory address zero and calls the function. The input length is zero with an empty permission list. Treat the input as bytes.

Write the UTF-8 result into memory. Return a value whose high 32 bits contain the byte address and whose low 32 bits contain the byte length. A result can occupy up to 16 KiB. PawCommander displays the result as selectable plain text and indicates when the input was limited to its prefix.

The interpreter runs in a separate process and accepts modules with an empty import list. The guest receives data through linear memory and has no host filesystem, network or process interface. Each run has a 4 MiB memory cap, a bounded table and a computation budget. PawCommander stops a helper that reaches its four-second response deadline. A bad package, trap or invalid result produces an error in the preview window.
