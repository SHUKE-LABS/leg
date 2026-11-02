/*
 * The libarchive build configuration used by LegHost.
 *
 * iSH keeps the generated libarchive configuration in deps/config.h. That
 * configuration enables the xar formats, which need libxml2. iOS has no
 * libxml2, so the xar reader and writer are compiled as the "not supported"
 * stubs instead. The guest root importer only reads and writes tar archives,
 * so nothing else changes.
 *
 * This file shadows deps/config.h for every translation unit that includes
 * "config.h": quoted includes resolve to the directory of the including file
 * first, and libarchive's own sources all live next to this file.
 */

#ifndef LEGHOST_LIBARCHIVE_CONFIG_H
#define LEGHOST_LIBARCHIVE_CONFIG_H

#include "../config.h"

#undef HAVE_LIBXML_XMLREADER_H
#undef HAVE_LIBXML_XMLWRITER_H

#endif /* LEGHOST_LIBARCHIVE_CONFIG_H */
