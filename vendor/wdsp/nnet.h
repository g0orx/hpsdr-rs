/*  nnet.h

Neural noise reduction - network layers and model assembly

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

#ifndef _nnet_h
#define _nnet_h

#include "nnio.h"

typedef struct _nnet* NNET;

#define NNET_MODE_NETWORK	0
#define NNET_MODE_IDENTITY	1
#define NNET_MODE_LOWPASS	2

#ifndef NNET_NSLOTS
#define NNET_NSLOTS 4
#endif

extern void destroy_nnet (NNET n);

extern void flush_nnet (NNET n);

extern void run_nnet
	(
	NNET n,
	const double* xre,
	const double* xim,
	double* yre,
	double* yim
	);

extern void setFloor_nnet (NNET n, double floor_db);

extern const double* getStage_nnet (NNET n, const char* name,
	int* nch, int* nfreq);

extern int isReady_nnet (NNET n);

// [Exported (PORT) functions of this module that nothing else in WDSP, nor
// ChannelMaster, calls are declared only in wdsp.h, the public header.]

// diagnostics
extern int getBins_nnet (NNET n);

extern int getLookahead_nnet (NNET n);

extern void setMode_nnet (NNET n, int mode);

extern int getMode_nnet (NNET n);


extern void setAlpha_nnet (NNET n, double alpha);

extern double getAlpha_nnet (NNET n);

extern void setKnee_nnet (NNET n, double knee_db);

extern double getKnee_nnet (NNET n);

extern void setTau_nnet (NNET n, double tau);

extern double getTau_nnet (NNET n);

extern void setMaxGain_nnet (NNET n, double gmax_db);

extern double getMaxGain_nnet (NNET n);

extern void setSmooth_nnet (NNET n, double att_ms, double rel_ms);

extern void getSmooth_nnet (NNET n, double* att_ms, double* rel_ms);


typedef struct _gru* GRU;

extern const double* getState_gru (GRU g);

extern void setState_gru (GRU g, const double* h);

typedef struct _conv2d* CONV2D;

extern int getCout_conv2d (CONV2D c);

typedef struct _convt2d* CONVT2D;

extern int getCout_convt2d (CONVT2D c);

typedef struct _dfhead* DFHEAD;

extern int getOrder_dfhead (DFHEAD d);

typedef struct _cond* COND;

extern double getGain_cond (COND c);

typedef struct _dprnn* DPRNN;

#ifdef NNET_PROFILE
extern void report_profile_nnet (void);
extern void reset_profile_nnet (void);
#endif

extern int ok_nnet (NNET n);

extern int getTrainingBand(NNET n, double* band_low, double* band_high);

extern NNET create_nnet_slot (int slot, int nbins, int lookahead,
                              double floor_db, double frame_rate,
	                          double band_low, double band_high);

extern int nnet_probe_slot (int slot, int* nbins, int* nrate, int* nfft,
	int* hop, int* lookahead, double* band_lo, double* band_hi);

#endif
