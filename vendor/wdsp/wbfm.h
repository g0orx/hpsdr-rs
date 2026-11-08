/*  wbfm.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2026 Warren Pratt, NR0V

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

#ifndef _wbfm_h
#define _wbfm_h

#include "firmin.h"
#include "wcpAGC.h"

//Forward declarations:
//   Full definitions expected elsewhere; this allows
//   wbfm.h to refer to these as pointer types.
typedef struct _wbsql  *WBSQL;
typedef struct _indy   *INDY;
typedef struct _dmph   *DMPH;
typedef struct _sdelay *SDELAY;


typedef struct _wbfm* WBFM;

extern WBFM create_wbfm(int run, int size, double* in, double* out, double rate);
extern void destroy_wbfm(WBFM a);
extern void xwbfm(WBFM a);
extern void flush_wbfm(WBFM a);
extern void setBuffers_wbfm(WBFM a, double* in, double* out);
extern void setSamplerate_wbfm(WBFM a, int rate);
extern void setSize_wbfm(WBFM a, int size);

extern void setRun_wbfm (WBFM a, int run);

#endif

