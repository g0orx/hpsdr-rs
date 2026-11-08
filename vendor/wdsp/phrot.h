/*  phrot.h

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

The author can be reached by email at  warren@pratt.one
*/

/*  Acknowledgment

    The auto-optimizer and waveform asymmetry display subsystem in this file
    were developed with the assistance of Claude (Anthropic), an AI assistant,
    in collaboration with Warren Pratt, NR0V.  The algorithms, design decisions,
    and final implementation are the original work of Warren Pratt.  Claude
    contributed to algorithm design, code drafting, debugging, and documentation
    during an iterative development process.

    Claude does not hold copyright.  All copyright in this file remains with
    Warren Pratt, NR0V, as stated above.
*/

#ifndef _phrot_h
#define _phrot_h

typedef struct _phrot* PHROT;
extern void flush_phrot  (PHROT a);
extern PHROT create_phrot  (int run, int size, double* in, double* out,
                             int rate, double fc, int nstages);
extern void  destroy_phrot (PHROT a);
extern void xphrot (PHROT a);
extern void setBuffers_phrot   (PHROT a, double* in, double* out);
extern void setSamplerate_phrot(PHROT a, int rate);
extern void setSize_phrot      (PHROT a, int size);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.]

#endif  // _phrot_h
