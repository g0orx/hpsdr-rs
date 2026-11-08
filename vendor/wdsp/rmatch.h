/*  rmatch.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2017, 2022 Warren Pratt, NR0V

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

#ifndef _rmatch_h
#define _rmatch_h

#include "varsamp.h"

typedef struct _mav* MAV;

typedef struct _aamav* AAMAV;

typedef struct _rmatch* RMATCH;

// [PORT is defined in wdsp_platform.h, which comm.h includes ahead of the module
// headers.  Some WDSP headers are also included on their own, ahead of comm.h
// (ChannelMaster includes resample.h this way), so a header that uses PORT
// includes wdsp_platform.h itself.  It also supplies the types, CRITICAL_SECTION
// and HANDLE, that some of these headers use.]
#include "wdsp_platform.h"

extern PORT void* create_rmatchV(int in_size, int out_size, int nom_inrate, int nom_outrate, int ringsize, double var);

extern PORT void destroy_rmatchV (void* ptr);

extern PORT void xrmatchOUT (void* b, double* out);

extern PORT void xrmatchIN (void* b, double* in);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.  Those
// declared here are called by other modules or by ChannelMaster.]

extern PORT void getRMatchDiags (void* b, int* underflows, int* overflows, double* var, int* ringsize, int* nring);

extern PORT void resetRMatchDiags (void* b);

extern PORT void forceRMatchVar (void* b, int force, double fvar);

extern PORT void setRMatchFeedbackGain(void* b, double feedback_gain);

extern PORT void setRMatchSlewTime1(void* b, double slew_time);

extern PORT void setRMatchPropRingMin(void* ptr, int prop_min);

extern PORT void setRMatchPropRingMax(void* ptr, int prop_max);

extern PORT void setRMatchFFRingMin(void* ptr, int ff_ringmin);

extern PORT void setRMatchFFRingMax(void* ptr, int ff_ringmax);

extern PORT void setRMatchFFAlpha(void* ptr, double ff_alpha);

extern PORT void getControlFlag(void* ptr, int* control_flag);

#endif
