/*  sbnr.c

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
sbnr.h's own updated header comment): the vendored libspecbleach
library this file used to call into (specbleach_adaptive_initialize/
process/free/load_parameters) has been removed from this project --
this noise-reduction stage is created but never enabled anywhere here
(no Rust code calls SetRXASBNRRun with a nonzero value; WDSP's built-
in neural-net NNR stage, `nnr.c`, is what's actually wired up to the
UI). Every function below keeps its original signature so RXA.c and
everything else that references `SBNR`/these functions needed no
changes -- only the real DSP work (which was dead code, exercised by
nothing) is gone. The per-parameter setters (SetRXASBNRreductionAmount
etc.) still store their values on the struct (harmless, never read by
anything now) rather than being removed, to keep this file's public
API surface identical to upstream.
*/

#define _CRT_SECURE_NO_WARNINGS

#include "comm.h"

void setSize_sbnr(SBNR a, int size) {
  _aligned_free(a->input);
  _aligned_free(a->output);
  a->input = malloc0(size * sizeof(float));
  a->output = malloc0(size * sizeof(float));
  a->buffer_size = size;
}

void setBuffers_sbnr(SBNR a, double *in, double *out) {
  a->in = in;
  a->out = out;
}

SBNR create_sbnr(int run, int position, int size, double *in, double *out, int rate) {
  SBNR a = (SBNR) malloc0(sizeof(sbnr));
  a->run = run;
  a->position = position;
  a->rate = rate;
  a->st = NULL;
  a->in = in;
  a->out = out;
  a->reduction_amount = 10.F;
  a->smoothing_factor = 0.F;
  a->whitening_factor = 0.F;
  a->noise_scaling_type = 0;
  a->noise_rescale = 2.F;
  a->post_filter_threshold = -10.F;
  a->buffer_size = size;
  a->input = malloc0(a->buffer_size * sizeof(float));
  a->output = malloc0(a->buffer_size * sizeof(float));
  return a;
}

void setSamplerate_sbnr(SBNR a, int rate) {
  a->rate = rate;
}

void xsbnr(SBNR a, int pos) {
  // Always the passthrough path -- a->run is permanently 0 (see this
  // file's own header comment), but kept branch-for-branch identical
  // to every other WDSP stage's own "not running" behavior rather
  // than special-casing it away.
  if (a->out != a->in) {
    memcpy(a->out, a->in, a->buffer_size * sizeof(complex));
  }
}

void destroy_sbnr(SBNR a) {
  _aligned_free(a->input);
  _aligned_free(a->output);
  _aligned_free(a);
}

PORT
void SetRXASBNRRun(int channel, int run) {
  SBNR a = rxa[channel].sbnr.p;
  if (a->run != run) {
    RXAbp1Check(channel, rxa[channel].amd.p->run, rxa[channel].snba.p->run,
                rxa[channel].emnr.p->run, getRun_nnr(rxa[channel].nnr.p),
                rxa[channel].anf.p->run, rxa[channel].anr.p->run, rxa[channel].rnnr.p->run, run);
    EnterCriticalSection(&ch[channel].csDSP);
    a->run = run;
    RXAbp1Set(channel);
    LeaveCriticalSection(&ch[channel].csDSP);
  }
}

/* Sets the amount of dBs that the noise will be attenuated. It goes from 0 dB
  * to 20 dB */
PORT
void SetRXASBNRreductionAmount(int channel, float amount) {
  if (amount < 0 || amount > 20) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->reduction_amount = amount;
  LeaveCriticalSection(&ch[channel].csDSP);
}

/* Percentage of smoothing to apply. Averages the reduction calculation frame
 * per frame so the rate of change is less resulting in less musical noise but
 * if too strong it can blur transient and reduce high frequencies. It goes
 * from 0 to 100 percent */
PORT
void SetRXASBNRsmoothingFactor(int channel, float factor) {
  if (factor < 0 || factor > 100) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->smoothing_factor = factor;
  LeaveCriticalSection(&ch[channel].csDSP);
}

/* Percentage of whitening that is going to be applied to the residue of the
 * reduction. It modifies the noise floor to be more like white noise. This
 * can help hide musical noise when the noise is colored. It goes from 0 to
 * 100 percent */
PORT
void SetRXASBNRwhiteningFactor(int channel, float factor) {
  if (factor < 0 || factor > 100) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->whitening_factor = factor;
  LeaveCriticalSection(&ch[channel].csDSP);
}

/* Strength in which the reduction will be applied. It uses the masking
 * thresholds of the signal to determine where in the spectrum the reduction
 * needs to be stronger. This parameter scales how much in each of the
 * frequencies the reduction is going to be applied. It can be a positive dB
 * value in between 0 dB and 12 dB */
PORT
void SetRXASBNRnoiseRescale(int channel, float factor) {
  if (factor < 0 || factor > 12) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->noise_rescale = factor;
  LeaveCriticalSection(&ch[channel].csDSP);
}

/* Sets the SNR threshold in dB in which the post-filter will start to blur
 * musical noise. It can be a positive or negative dB value in between -10 dB
 * and 10 dB */
PORT
void SetRXASBNRpostFilterThreshold(int channel, float threshold) {
  if (threshold < -10 || threshold > 10) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->post_filter_threshold = threshold;
  LeaveCriticalSection(&ch[channel].csDSP);
}

/* Type of algorithm used to scale noise in order to apply over or under
 * subtraction in different parts of the spectrum while calculating the
 * reduction. 0 is a-posteriori snr scaling using the complete spectrum, 1 is
 * a-posteriori using critical bands and 2 is using masking thresholds
 */
PORT
void SetRXASBNRnoiseScalingType(int channel, int noise_scaling_type) {
  if (noise_scaling_type < 0 || noise_scaling_type > 2) { return; }
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->noise_scaling_type = noise_scaling_type;
  LeaveCriticalSection(&ch[channel].csDSP);
}

PORT
void SetRXASBNRPosition(int channel, int position) {
  EnterCriticalSection(&ch[channel].csDSP);
  rxa[channel].sbnr.p->position = position;
  rxa[channel].bp1.p->position = position;
  LeaveCriticalSection(&ch[channel].csDSP);
}
