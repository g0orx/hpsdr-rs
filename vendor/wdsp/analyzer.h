/*  analyzer.h

This file is part of a program that implements a Spectrum Analyzer
used in conjunction with software-defined-radio hardware.

Copyright (C) 2012, 2013, 2014, 2016, 2023, 2025 Warren Pratt, NR0V

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
#ifndef _analyzer_h
#define _analyzer_h
#include "comm.h"

typedef struct _dp* DP;

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

extern PORT
void XCreateAnalyzer (	int disp,
						int *success,	//writes '0' to success if all went well, <0 if mem alloc failed
						int m_size,		//maximum fft size to be used
						int m_LO,		//maximum number of LO positions per subspan
						int m_stitch,	//maximum number of subspans to be concatenated
						char *app_data_path
					 );

extern PORT   
void DestroyAnalyzer(int disp);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.  Those
// declared here are called by other modules or by ChannelMaster.]

extern PORT
void Spectrum(int disp, int ss, int LO, dINREAL* pI, dINREAL* pQ);

extern PORT
void Spectrum2(int run, int disp, int ss, int LO, dINREAL* pbuff);

extern PORT
void Spectrum0(int run, int disp, int ss, int LO, double* pbuff);

#endif