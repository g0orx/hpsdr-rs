/*  snb.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2015, 2016 Warren Pratt, NR0V

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

#ifndef _snba_h
#define _snba_h

#include "resample.h"

typedef struct _snba* SNBA;

extern SNBA create_snba (int run, double* in, double* out, int inrate, int internalrate, int bsize, int ovrlp, int xsize,
	int asize, int npasses, double k1, double k2, int b, int pre, int post, double pmultmin, double out_low_cut, double out_high_cut);

extern void destroy_snba (SNBA d);

extern void flush_snba (SNBA d);

extern void xsnba (SNBA d);

extern void setBuffers_snba (SNBA a, double* in, double* out);

extern void setSamplerate_snba (SNBA a, int rate);

extern void setSize_snba (SNBA a, int size);

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

PORT void SetRXASNBAOutputBandwidth (int channel, double flow, double fhigh);

typedef struct _bpsnba* BPSNBA;

extern BPSNBA create_bpsnba (int run, int run_notches, int position, int size, int nc, int mp, double* in, double* out, int rate,  
	double abs_low_freq, double abs_high_freq, double f_low, double f_high, int wintype, double gain, int autoincr, 
	int maxpb, NOTCHDB* ptraddr);

extern void destroy_bpsnba (BPSNBA a);

extern void flush_bpsnba (BPSNBA a);

extern void setBuffers_bpsnba (BPSNBA a, double* in, double* out);

extern void setSamplerate_bpsnba (BPSNBA a, int rate);

extern void setSize_bpsnba (BPSNBA a, int size);

extern void xbpsnbain (BPSNBA a, int position);

extern void xbpsnbaout (BPSNBA a, int position);

extern void recalc_bpsnba_filter (BPSNBA a, int update);

PORT void RXABPSNBASetNC (int channel, int nc);

PORT void RXABPSNBASetMP (int channel, int mp);

extern int getRun_snba (SNBA a);

extern double getAbsLowFreq_bpsnba (BPSNBA a);

extern double getAbsHighFreq_bpsnba (BPSNBA a);

extern double getFLow_bpsnba (BPSNBA a);

extern void setFLow_bpsnba (BPSNBA a, double f_low);

extern double getFHigh_bpsnba (BPSNBA a);

extern void setFHigh_bpsnba (BPSNBA a, double f_high);

extern int getRunNotches_bpsnba (BPSNBA a);

extern void setRunNotches_bpsnba (BPSNBA a, int run_notches);

extern void setRun_bpsnba (BPSNBA a, int run);

extern void setPosition_bpsnba (BPSNBA a, int position);

extern void setUpdate_bpsnba (BPSNBA a);

extern NBP getNbp_bpsnba (BPSNBA a);

extern int getWintype_bpsnba (BPSNBA a);

extern void setWintype_bpsnba (BPSNBA a, int wintype);

extern int getAutoincr_bpsnba (BPSNBA a);

extern void setAutoincr_bpsnba (BPSNBA a, int autoincr);

#endif
