/*  rnnr.h

This file is part of a program that implements a Software-Defined Radio.

This code/file can be found on GitHub : https://github.com/ramdor/Thetis

Copyright (C) 2000-2025 Original authors
Copyright (C) 2020-2025 Richard Samphire MW0LGE

This program is free software; you can redistribute it and/or
modify it under the terms of the GNU General Public License
as published by the Free Software Foundation; either version 2
of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program; if not, write to the Free Software
Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.

The author can be reached by email at

mw0lge@grange-lane.co.uk

Original code is based on code and ideas from : https://github.com/vu3rdd/wdsp
and used RNNoise (https://gitlab.xiph.org/xiph/rnnoise).

STUBBED (2026-09-08, see this project's memory/wdsp_210_port.md): the
vendored RNNoise library this file originally called into has been
removed -- this stage is never enabled anywhere in this project (no
Rust code calls SetRXARNNRRun with a nonzero value; `nnr.c`'s neural-
net NNR stage is what's actually wired up to the UI, see spectrum.rs's
NoiseReduction doc comment), so the real DSP work was dead code kept
alive only by a library dependency nothing used. This header keeps
every symbol RXA.c/RXA.h and the rest of WDSP already reference
(struct layout, function signatures) so nothing outside rnnr.c/rnnr.h
needed to change -- only the actual RNNoise-backed implementation is
gone, replaced with an inert passthrough in rnnr.c.
*/

#ifndef _rnnr_h
#define _rnnr_h

typedef struct _rnnr {
  int run;
  int run_old; // unused now -- kept for struct-layout compatibility
  int position;
  int frame_size;
  void *st; // was RNNoise's DenoiseState*; unused now, kept for layout
  double *in;
  double *out;
  float gain;
  float gain_db;
  float agc_att_a;
  float agc_rel_a;

  int buffer_size;
  int rate;
  float *output_buffer;

  float *to_process_buffer;
  float *processed_output_buffer;

  CRITICAL_SECTION cs;

} rnnr, *RNNR;

extern RNNR create_rnnr(int run, int position, int size, double *in, double *out, int rate);
extern void setSize_rnnr(RNNR a, int size);
extern void setBuffers_rnnr(RNNR a, double *in, double *out);
extern void destroy_rnnr(RNNR a);
extern void xrnnr(RNNR a, int pos);
extern void setSamplerate_rnnr(RNNR a, int rate);

#endif //_rnnr_h
