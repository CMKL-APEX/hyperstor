/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Copyright (C) 2026 The Hyperstor authors
 *
 * This file may be redistributed under the terms of the
 * GNU Lesser General Public License.
 *
 * Hyperstor's device superblock (https://github.com/CMKL-APEX/hyperstor,
 * spec/08-engine.md section 7.1): block 0 of a device Hyperstor formats
 * whole, a copy in block 1. All integers are little-endian:
 *
 *   0   magic "HSDEVC01" as a u64 (the bytes "10CVEDSH")
 *   8   format version, u32: 1, or 2 with feature flags
 *   12  (version 2 only) compat, ro_compat, incompat flags, u64 each
 *   then device id, u128 (the cluster map's device number)
 *        cluster id, u128 (the same on every device of a cluster)
 *        size in 4 KiB blocks, u64
 *        regions: a u32 count, then 24 bytes each
 *        CRC32C of everything before it, u32
 */
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <inttypes.h>

#include "superblocks.h"
#include "crc32c.h"

#define HYPERSTOR_MAGIC		"10CVEDSH"
#define HYPERSTOR_BLOCK		4096
#define HYPERSTOR_REGION	24

static uint32_t le32_at(const unsigned char *b)
{
	return (uint32_t) b[0] | (uint32_t) b[1] << 8 |
	       (uint32_t) b[2] << 16 | (uint32_t) b[3] << 24;
}

static uint64_t le64_at(const unsigned char *b)
{
	return (uint64_t) le32_at(b) | (uint64_t) le32_at(b + 4) << 32;
}

static int probe_hyperstor(blkid_probe pr,
		const struct blkid_idmag *mag __attribute__((__unused__)))
{
	const unsigned char *sb;
	unsigned char uuid[16];
	char label[32];
	uint32_t version, regions, crc;
	size_t off, end;
	int i;

	sb = blkid_probe_get_buffer(pr, 0, HYPERSTOR_BLOCK);
	if (!sb)
		return errno ? -errno : 1;

	version = le32_at(sb + 8);
	if (version == 1)
		off = 12;
	else if (version == 2)
		off = 12 + 3 * 8;
	else
		return 1;

	/* device id, cluster id, blocks, then the region count */
	if (off + 16 + 16 + 8 + 4 > HYPERSTOR_BLOCK)
		return 1;
	regions = le32_at(sb + off + 40);
	if (regions > (HYPERSTOR_BLOCK - off - 44 - 4) / HYPERSTOR_REGION)
		return 1;
	end = off + 44 + (size_t) regions * HYPERSTOR_REGION;
	crc = le32_at(sb + end);
	if (blkid_probe_verify_csum(pr, ~crc32c(~0U, sb, end), crc) == 0)
		return 1;

	/* The cluster id as the UUID (shared, as btrfs shares its fs UUID
	 * across a filesystem's devices), its u128 written big-endian so the
	 * UUID reads as the number in hex. */
	for (i = 0; i < 16; i++)
		uuid[i] = sb[off + 16 + 15 - i];
	blkid_probe_set_uuid(pr, uuid);
	blkid_probe_sprintf_version(pr, "%" PRIu32, version);
	/* The device id: the low 64 bits are the map's device number. */
	snprintf(label, sizeof(label), "device-%" PRIu64, le64_at(sb + off));
	blkid_probe_set_label(pr, (unsigned char *) label, strlen(label));
	blkid_probe_set_fsblocksize(pr, HYPERSTOR_BLOCK);
	blkid_probe_set_block_size(pr, HYPERSTOR_BLOCK);
	return 0;
}

const struct blkid_idinfo hyperstor_idinfo =
{
	.name		= "hyperstor",
	.usage		= BLKID_USAGE_OTHER,
	.probefunc	= probe_hyperstor,
	.minsz		= 3 * HYPERSTOR_BLOCK,
	.magics		=
	{
		{ .magic = HYPERSTOR_MAGIC, .len = 8, .kboff = 0, .sboff = 0 },
		{ NULL }
	}
};
