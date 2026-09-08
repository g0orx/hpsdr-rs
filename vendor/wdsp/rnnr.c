/*  rnnr.c

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

STUBBED (2026-09-08, see this project's memory/wdsp_210_port.md and
rnnr.h's own updated header comment): the vendored RNNoise library
this file used to call into (rnnoise_create/process_frame/destroy,
rnnoise_model_from_filename/free) has been removed from this project
-- this noise-reduction stage is created but never enabled anywhere
here (no Rust code calls SetRXARNNRRun with a nonzero value; WDSP's
built-in neural-net NNR stage, `nnr.c`, is what's actually wired up to
the UI). Every function below keeps its original signature so RXA.c
and everything else that references `RNNR`/these functions needed no
changes -- only the real DSP work (which was dead code, exercised by
nothing) is gone.
*/

#define _CRT_SECURE_NO_WARNINGS
#include "comm.h"

PORT
void SetRXARNNRRun(int channel, int run) {
  RNNR a = rxa[channel].rnnr.p;
  if (a->run != run) {
    RXAbp1Check(channel, rxa[channel].amd.p->run, rxa[channel].snba.p->run,
                rxa[channel].emnr.p->run, getRun_nnr(rxa[channel].nnr.p),
                rxa[channel].anf.p->run, rxa[channel].anr.p->run, run, rxa[channel].sbnr.p->run);
    EnterCriticalSection(&ch[channel].csDSP);
    a->run = run;
    RXAbp1Set(channel);
    LeaveCriticalSection(&ch[channel].csDSP);
  }
}

void setSize_rnnr(RNNR a, int size) {
  _aligned_free(a->output_buffer);
  a->buffer_size = size;
  a->output_buffer = malloc0(a->buffer_size * sizeof(float));
}

void setBuffers_rnnr(RNNR a, double *in, double *out) {
  a->in = in;
  a->out = out;
}

void setSamplerate_rnnr(RNNR a, int rate) {
  a->rate = rate;
}

RNNR create_rnnr(int run, int position, int size, double *in, double *out, int rate) {
  RNNR a = malloc0(sizeof(rnnr));
  InitializeCriticalSection(&a->cs);
  a->run = run;
  a->position = position;
  a->rate = rate;
  a->st = NULL;
  a->frame_size = 0;
  a->in = in;
  a->out = out;
  a->buffer_size = size;
  a->gain = 1.0f;
  a->gain_db = 0.0f;
  a->output_buffer = malloc0(a->buffer_size * sizeof(float));
  return a;
}

void xrnnr(RNNR a, int pos) {
  // Always the passthrough path -- a->run is permanently 0 (see this
  // file's own header comment), but kept branch-for-branch identical
  // to every other WDSP stage's own "not running" behavior rather
  // than special-casing it away.
  if (a->out != a->in) {
    memcpy(a->out, a->in, a->buffer_size * sizeof(complex));
  }
}

void destroy_rnnr(RNNR a) {
  DeleteCriticalSection(&a->cs);
  _aligned_free(a->output_buffer);
  _aligned_free(a);
}

PORT
void RNNRloadModel(const char *file_path) {
  // No-op -- the model this loaded was RNNoise's, which no longer
  // exists in this project (see this file's own header comment).
}

PORT
void SetRXARNNRPosition(int channel, int position) {
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].rnnr.p->position = position;
  rxa[channel].bp1.p->position = position;
  LeaveCriticalSection(&ch[channel].csDSP);
}
