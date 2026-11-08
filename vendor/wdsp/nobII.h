/*  nobII.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2014 Warren Pratt, NR0V

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

#ifndef _nob_h
#define _nob_h

typedef struct _nob* NOB;

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

PORT NOB create_nob	(
	int run,
	int buffsize,
	double* in,
	double* out,
	double samplerate,
	int mode,
	double advslewtime,
	double advtime,
	double hangslewtime,
	double hangtime,
	double max_imp_seq_time,
	double backtau,
	double threshold
						);

PORT void destroy_nob (NOB a);

PORT void flush_nob (NOB a);

PORT void xnob (NOB a);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.  Those
// declared here are called by other modules or by ChannelMaster.]

extern void setBuffers_nob (NOB a, double* in, double* out);

extern void setSamplerate_nob (NOB a, int rate);

extern void setSize_nob (NOB a, int size);


extern PORT void pSetRCVRNOBRun (NOB a, int run);

extern PORT void pSetRCVRNOBMode (NOB a, int mode);

extern PORT void pSetRCVRNOBBuffsize (NOB a, int size);

extern PORT void pSetRCVRNOBSamplerate (NOB a, int size);

extern PORT void pSetRCVRNOBTau (NOB a, double tau);

extern PORT void pSetRCVRNOBHangtime (NOB a, double time);

extern PORT void pSetRCVRNOBAdvtime (NOB a, double time);

extern PORT void pSetRCVRNOBBacktau (NOB a, double tau);

extern PORT void pSetRCVRNOBThreshold (NOB a, double thresh);

#endif