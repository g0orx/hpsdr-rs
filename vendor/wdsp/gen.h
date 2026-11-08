/*  gen.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2013, 2025 Warren Pratt, NR0V

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

#ifndef _gen_h
#define _gen_h

typedef struct _gen* GEN;

extern GEN create_gen (int run, int size, double* in, double* out, int rate, int mode);

extern void destroy_gen (GEN a);

extern void flush_gen (GEN a);

extern void xgen (GEN a);

extern void setBuffers_gen (GEN a, double* in, double* out);

extern void setSamplerate_gen (GEN a, int rate);

extern void setSize_gen (GEN a, int size);

extern int getRun_gen (GEN a);

// TXA Properties


#endif