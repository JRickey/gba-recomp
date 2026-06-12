# Third-party vendored assets

This file records third-party content checked into the repository
verbatim (as data, not as a code dependency). Code dependencies are
covered by the README's Legal section and the per-dependency licenses
bundled with release distributions; this file is for vendored files.

## `assets/gamecontrollerdb.txt`

- **What:** A community-maintained database of game-controller button and
  axis mappings, in the SDL game-controller mapping format. Bundled so
  the launcher and the play runtime recognize the broadest possible set
  of controllers, including ones newer than the snapshot the `gilrs`
  dependency ships with. Embedded into both binaries via `include_str!`,
  so the packaged product carries it with no external file dependency.
- **Source:** SDL_GameControllerDB —
  <https://github.com/mdqinc/SDL_GameControllerDB>
- **Upstream commit:** `998d5b08b5b33bdf3a63b2ef8f2ac4ccc664e2f6`
  (master, retrieved 2026-06-12).
- **License:** zlib License — compatible with this project's
  MIT-OR-Apache-2.0 licensing; not copyleft. The full upstream license
  text follows verbatim, preserved here because the zlib license requires
  the notice to travel with redistributed copies.

```
Copyright (C) 1997-2025 Sam Lantinga <slouken@libsdl.org>

This software is provided 'as-is', without any express or implied
warranty.  In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.
```
