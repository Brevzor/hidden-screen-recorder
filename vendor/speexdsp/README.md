SpeexDSP 1.2.1, from https://github.com/xiph/speexdsp
Tag: SpeexDSP-1.2.1; commit: 1b28a0f61bc31162979e1f26f3981fc3637095c8.

Only the preprocessing/echo/FFT sources and headers needed by the static
noise suppression build are included. Sources are unmodified. The build
uses floating point and SMALLFT, without automatic gain control or VAD.
No external DLL, CMake or bindgen is required. See COPYING and individual
source notices for the BSD licenses.
