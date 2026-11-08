/*  emnr.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2015, 2026 Warren Pratt, NR0V

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

#ifndef _emnr_h
#define _emnr_h

typedef struct _emnr* EMNR;

extern EMNR create_emnr (int run, int position, int size, double* in, double* out, int fsize, int ovrlp, 
	int rate, int wintype, double gain, int gain_method, int npe_method, int ae_run);

extern void destroy_emnr (EMNR a);

extern void flush_emnr (EMNR a);

extern void xemnr (EMNR a, int pos);

extern void setBuffers_emnr (EMNR a, double* in, double* out);

extern void setSamplerate_emnr (EMNR a, int rate);

extern void setSize_emnr (EMNR a, int size);

extern int getRun_emnr (EMNR a);

extern int getPosition_emnr (EMNR a);

#endif