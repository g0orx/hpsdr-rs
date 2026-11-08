/*  nbp.h

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

#ifndef _nbp_h
#define _nbp_h
#include "firmin.h"
typedef struct _notchdb* NOTCHDB;

extern NOTCHDB create_notchdb (int master_run, int maxnotches);

extern void destroy_notchdb (NOTCHDB b);

typedef struct _nbp* NBP;

extern NBP create_nbp(int run, int fnfrun, int position, int size, int nc, int mp, double* in, double* out, 
	double flow, double fhigh, int rate, int wintype, double gain, int autoincr, int maxpb, NOTCHDB* ptraddr);

extern void destroy_nbp (NBP a);

extern void flush_nbp (NBP a);

extern void xnbp (NBP a, int pos);

extern void setBuffers_nbp (NBP a, double* in, double* out);

extern void setSamplerate_nbp (NBP a, int rate);

extern void setSize_nbp (NBP a, int size);

extern void setNc_nbp (NBP a, int nc);

extern void setMp_nbp (NBP a, int mp);

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

PORT void RXANBPSetFreqs (int channel, double flow, double fhigh);

PORT void RXANBPSetNC (int channel, int nc);

PORT void RXANBPSetMP (int channel, int mp);

extern int getMasterRun_notchdb (NOTCHDB a);

extern void setRun_nbp (NBP a, int run);

extern double getFlow_nbp (NBP a);

extern double getFhigh_nbp (NBP a);

extern void setFnfrun_nbp (NBP a, int fnfrun);

extern void setFlow_nbp (NBP a, double flow);

extern void setFhigh_nbp (NBP a, double fhigh);

extern void setWintype_nbp (NBP a, int wintype);

extern void setGain_nbp (NBP a, double gain);

extern void setAutoincr_nbp (NBP a, int autoincr);

extern void setImpulse_nbp (NBP a, int update);

extern void setUpdate_nbp (NBP a);

#endif
