/*  ssql.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2023 Warren Pratt, NR0V

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

#ifndef _ssql_h
#define _ssql_h

typedef struct _ftov* FTOV;

typedef struct _ssql* SSQL;

extern SSQL create_ssql (int run, int size, double* in, double* out, int rate, double tup, double tdown,
	double muted_gain, double tau_mute, double tau_unmute, double wthresh, double tr_thresh, int rsize, double fmax);

extern void destroy_ssql (SSQL a);

extern void flush_ssql (SSQL a);

extern void xssql (SSQL a);

extern void setBuffers_ssql (SSQL a, double* in, double* out);

extern void setSamplerate_ssql (SSQL a, int rate);

extern void setSize_ssql (SSQL a, int size);

#endif
