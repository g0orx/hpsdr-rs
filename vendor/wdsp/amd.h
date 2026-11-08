/*  amd.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2012, 2013 Warren Pratt, NR0V

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

#ifndef _amd_h
#define _amd_h

// ff defines for sbdemod
#ifndef STAGES
#define STAGES		7
#endif

#ifndef OUT_IDX
#define OUT_IDX		(3 * STAGES)
#endif

typedef struct _amd* AMD;

extern AMD create_amd 
	(
	int run,
	int buff_size,
	double *in_buff,
	double *out_buff,
	int mode,
	int levelfade,
	int sbmode,
	int sample_rate,
	double fmin,
	double fmax,
	double zeta,
	double omegaN,
	double tauR,
	double tauI
	);

extern void destroy_amd (AMD a);

extern void flush_amd (AMD a);

extern void xamd (AMD a);

extern void setBuffers_amd (AMD a, double* in, double* out);

extern void setSamplerate_amd (AMD a, int rate);

extern void setSize_amd (AMD a, int size);

extern int getRun_amd (AMD a);

extern void setRun_amd (AMD a, int run);

extern void setMode_amd (AMD a, int mode);

// RXA Properties

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.]

#endif