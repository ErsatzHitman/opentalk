# Third-party notices

## OpenWhispr

OpenTalk re-implements the dictation experience of [OpenWhispr](https://github.com/OpenWhispr/openwhispr) in Rust:
the recording pill's look and animations (waveform sampling and easing, the liquid cancel button), the cleanup and
rewrite prompts, provider behaviour and several text-processing rules follow OpenWhispr's design and code.
OpenWhispr is distributed under the following licence:

```
MIT License

Copyright (c) 2024 OpenWhispr Team

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.```

## Rust crates

`opentalk.exe` statically links the Rust crates listed in `Cargo.lock`. All of them are under permissive licences
(MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, Unicode-3.0, 0BSD or Unlicense). `cargo about` or
`cargo license` lists each crate with its licence text.
