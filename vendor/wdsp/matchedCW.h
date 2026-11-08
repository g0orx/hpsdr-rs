/*  matchedCW.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2025 Warren Pratt, NR0V

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

warren@pratt.one
*/

/********************************************************************************************************
*																										*
*									Partitioned Overlap-Save Gaussian									*
*																										*
********************************************************************************************************/


#ifndef _matchedCW_h
#define _matchedCW_h
#include "firmin.h"
typedef struct _matchedCW* MATCHED;

extern MATCHED create_matched (int run, int position, int size, double* in, double* out,
	double f_center, double bandwidth, int samplerate, double gain, int mode);

extern void destroy_matched (MATCHED a);

extern void flush_matched (MATCHED a);

extern void xmatched (MATCHED a, int pos);

extern void setBuffers_matched (MATCHED a, double* in, double* out);

extern void setSamplerate_matched (MATCHED a, int rate);

extern void setSize_matched (MATCHED a, int size);

extern void setGain_matched (MATCHED a, double gain);

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

extern PORT void SetRXAMatchedRun (int channel, int run);

extern PORT void SetRXAMatchedFreqs (int channel, double f_center, double bandwidth);

extern PORT void SetRXAMatchedGain (int channel, double gain);

#endif
