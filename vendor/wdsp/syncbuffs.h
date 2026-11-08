/*  cmbuffs.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2018 Warren Pratt, NR0V

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

#ifndef _syncbuffs_h
#define _syncbuffs_h
#include "comm.h"

#define SYNCB_MULT		(3)						
typedef struct _syncb* SYNCB;

extern SYNCB create_syncbuffs (int accept, int nstreams, int max_insize, int max_outsize, int outsize, double** out, void (*exf)(void));

extern void destroy_syncbuffs (SYNCB a);

extern void Syncbound (SYNCB a, int nsamples, double** in);	

extern void SetSYNCBRingOutsize (SYNCB a, int size);

#endif

/********************************************************************************************************
*																										*
*											Dummy Filter												*
*																										*
********************************************************************************************************/

#ifndef _dumfilt_h
#define _dumfilt_h

typedef struct _dumfilt* DUMFILT;

extern DUMFILT create_dumfilt (int run, int delay, int opsize, double* in, double* out);

extern void destroy_dumfilt (DUMFILT a);

extern void flush_dumfilt (DUMFILT a);

extern void xdumfilt (DUMFILT a);

#endif