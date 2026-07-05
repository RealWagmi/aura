# Third-party licenses

aura's prebuilt binaries statically link third-party open-source code. The
BSD-3-Clause license requires that redistributions in binary form reproduce
the copyright notice, the list of conditions, and the disclaimer in the
documentation and/or other materials provided with the distribution — this
file is that reproduction. It ships inside every release archive and in the
source repository.

For the full license inventory of all Rust dependencies, run
`cargo license` (or `cargo tree`) in the source tree; the entries below are
the components whose licenses require verbatim reproduction.

---

## sonora — pure-Rust WebRTC audio processing (AEC3 echo cancellation, noise suppression)

- Project: <https://github.com/dignifiedquire/sonora>
- Crates: `sonora`, `sonora-aec3`, `sonora-agc2`, `sonora-ns`,
  `sonora-common-audio`, `sonora-simd`, `sonora-fft`
- Used by: `aura-cli` (the client's anti-echo stage in `aura-audio`)
- License: BSD-3-Clause
- sonora is a Rust port of the WebRTC project's audio-processing module
  (by way of the `webrtc-audio-processing` packaging), so its license carries
  the upstream WebRTC and packaging copyrights alongside the port's own.

Full license text (verbatim from the project's `LICENSE` file):

```
Copyright (c) 2011, The WebRTC Project Authors. All rights reserved.
Copyright (c) 2016, Arun Raghavan and contributors. All rights reserved.
Copyright (c) 2026, dignifiedquire. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of Google nor the names of its contributors may
    be used to endorse or promote products derived from this software
    without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
