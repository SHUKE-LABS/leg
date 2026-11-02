/*
 * The Objective-C bridge for the LegHost app.
 *
 * The host talks to the deterministic guest provisioner and to the vendored
 * iSH runtime through these C interfaces. Nothing else from the guest is
 * exposed to Swift.
 */

#ifndef LEGHOST_BRIDGING_HEADER_H
#define LEGHOST_BRIDGING_HEADER_H

#include "GuestProvision.h"
#include "GuestRuntime.h"
#include "Sha256.h"

#endif /* LEGHOST_BRIDGING_HEADER_H */
