/*  lmath.h

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2015, 2023 Warren Pratt, NR0V

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

extern void trI (
    int n,
    double* r,
    double* B,
	double* y,
	double* v,
	double* dR_z
    );

extern void asolve(int xsize, int asize, double* x, double* a, double* r, double* z);

extern void median(int n, double* a, double* med);

#ifndef _bldr_h
#define _bldr_h

typedef struct _bldr* BLDR;

extern BLDR create_builder(int points, int ints);

extern void destroy_builder(BLDR a);

extern void xbuilder(BLDR a, int points, double* x, double* y, int ints, double* t, int* info, double* c, double ptol);

#endif