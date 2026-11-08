/*  wcpAGC.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2011, 2012, 2013 Warren Pratt, NR0V

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

warren@wpratt.com

*/

#ifndef _wcpagc_h
#define _wcpagc_h

#define MAX_SAMPLE_RATE		(384000.0)
#define MAX_N_TAU			(8)
#define MAX_TAU_ATTACK		(0.01)
#define RB_SIZE				(int)(MAX_SAMPLE_RATE * MAX_N_TAU * MAX_TAU_ATTACK + 1)


typedef struct _wcpagc* WCPAGC;

extern void xwcpagc (WCPAGC a);

extern WCPAGC create_wcpagc (	int run,
								int mode,
								int pmode,
								double* in,
								double* out,
								int io_buffsize,
								int sample_rate,
								double tau_attack,
								double tau_decay,
								int n_tau,
								double max_gain,
								double var_gain,
								double fixed_gain,
								double max_input,
								double out_targ,
								double tau_fast_backaverage,
								double tau_fast_decay,
								double pop_ratio,
								int hang_enable,
								double tau_hang_backmult,
								double hangtime,
								double hang_thresh,
								double tau_hang_decay
								);

extern void destroy_wcpagc (WCPAGC a);

extern void flush_wcpagc (WCPAGC a);

extern void setBuffers_wcpagc (WCPAGC a, double* in, double* out);

extern void setSamplerate_wcpagc (WCPAGC a, int rate);

extern void setSize_wcpagc (WCPAGC a, int size);

extern void setRun_wcpagc (WCPAGC a, int run);

extern int* getRunPtr_wcpagc (WCPAGC a);

extern double* getGainPtr_wcpagc (WCPAGC a);

// RXA Properties

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.]

#endif

