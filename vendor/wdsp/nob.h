/*  nob.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2013, 2014 Warren Pratt, NR0V

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

#ifndef _anb_h
#define _anb_h

typedef struct _anb* ANB;

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

PORT ANB create_anb	(
	int run,
	int buffsize,
	double* in,
	double* out,
	double samplerate,
	double tau,
	double hangtime,
	double advtime,
	double backtau,
	double threshold
						);

PORT void destroy_anb (ANB a);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.  Those
// declared here are called by other modules or by ChannelMaster.]

PORT void xanb (ANB a);

extern void setBuffers_anb (ANB a, double* in, double* out);

extern void setSamplerate_anb (ANB a, int rate);

extern void setSize_anb (ANB a, int size);


extern PORT void pSetRCVRANBRun (ANB a, int run);

extern PORT void pSetRCVRANBBuffsize (ANB a, int size);

extern PORT void pSetRCVRANBSamplerate (ANB a, int rate);

extern PORT void pSetRCVRANBTau (ANB a, double tau);

extern PORT void pSetRCVRANBHangtime (ANB a, double time);

extern PORT void pSetRCVRANBAdvtime (ANB a, double time);

extern PORT void pSetRCVRANBBacktau (ANB a, double tau);

extern PORT void pSetRCVRANBThreshold (ANB a, double thresh);

#endif