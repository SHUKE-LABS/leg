#ifndef ISH_INTERNAL
#error "for internal use only"
#endif

#ifndef FS_FAKE_H
#define FS_FAKE_H

#include "kernel/fs.h"
#include "fs/fake-db.h"
#include "misc.h"

struct fd *fakefs_open_inode(struct mount *mount, ino_t inode);

// LegHost: makes the node and, when `made` is not NULL, fills it with the
// stat of that node in the same transaction, so no other change to the mount
// comes between. A node whose stat fails is not made.
int fakefs_mknod_stat(struct mount *mount, const char *path, mode_t_ mode, dev_t_ dev, struct statbuf *made);

#endif
