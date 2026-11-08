/*  dexp.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2018, 2019 Warren Pratt, NR0V

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

#ifndef _dexp_h
#define _dexp_h

typedef struct _delring* DELRING;

typedef struct _dexp* DEXP;

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

PORT void create_dexp (int id, int run_dexp, int size, double* in, double* out, int rate, double dettau, double tattack, double tdecay, 
	double thold, double exp_ratio, double hyst_ratio, double attack_thresh, int nc, int wtype, double lowcut, double highcut, 
	int run_filt, int run_vox, int run_audelay, double audelay, void (__stdcall *pushvox)(int id, int active),
	int antivox_run, int antivox_size, int antivox_rate, double antivox_gain, double antivox_tau);

PORT void destroy_dexp (int id);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.  Those
// declared here are called by other modules or by ChannelMaster.]

PORT void xdexp (int id);

PORT void SetDEXPSize (int id, int size);

PORT void SetDEXPRate (int id, double rate);

PORT void SendAntiVOXData (int id, int nsamples, double* data);

#endif