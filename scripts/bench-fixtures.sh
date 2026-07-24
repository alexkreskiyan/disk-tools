#!/usr/bin/env bash
#
# Generate the three benchmark fixtures used by `just bench`.
#
#   usage: scripts/bench-fixtures.sh <dir>
#
# The shapes are deliberately different, because a disk scanner's cost splits
# between metadata traversal and per-file sizing:
#
#   node_modules/  ~105k tiny files, deeply nested  → metadata-bound worst case
#   cache/         ~20k files, 4 KiB–2 MiB, nested  → the mixed middle
#   media/         40 flat files of 200 MiB         → few entries, many bytes
#
# Files carry real bytes rather than being `truncate`d into sparse files: a
# sparse fixture reports ~0 allocated bytes, which would make the sizes in the
# report meaningless even though the walk timings would look the same.
#
# Costs roughly 10 GB and a few minutes. The target directory is created if
# missing and must be empty or non-existent — this never deletes anything.
set -euo pipefail

readonly TARGET="${1:?usage: bench-fixtures.sh <dir>}"

# Fixture dimensions. Kept here so the kb benchmark note can cite them.
readonly NM_PACKAGES=300
readonly NM_FILES_PER_PACKAGE=350
readonly CACHE_FILES=20000
readonly MEDIA_FILES=40
readonly MEDIA_MB=200

if [[ -e "$TARGET" && -n "$(ls -A "$TARGET" 2>/dev/null)" ]]; then
    echo "bench-fixtures: $TARGET exists and is not empty — refusing to touch it" >&2
    exit 1
fi
mkdir -p "$TARGET"

# A shell loop over 100k files spends minutes in fork/exec; perl does the whole
# fixture in one process.
generate_small_tree() {
    local root=$1 dirs=$2 files_per_dir=$3 min_bytes=$4 max_bytes=$5 nesting=$6

    ROOT=$root DIRS=$dirs FILES=$files_per_dir \
    MIN=$min_bytes MAX=$max_bytes NESTING=$nesting perl -e '
        use strict; use warnings;
        use File::Path qw(make_path);

        my ($root, $dirs, $files) = ($ENV{ROOT}, $ENV{DIRS}, $ENV{FILES});
        my ($min, $max, $nesting) = ($ENV{MIN}, $ENV{MAX}, $ENV{NESTING});
        # Fixed seed: two runs of this script produce byte-identical fixtures,
        # so timings from different days stay comparable.
        srand(20260725);

        for my $d (1 .. $dirs) {
            # Spread files over 1..$nesting levels below the package directory,
            # so the walk actually recurses instead of hitting one enormous
            # directory. `1 + $d % $nesting` cycles 1..$nesting; a bare
            # `$d % $nesting` would cycle 0..$nesting-1 and leave a fifth of the
            # packages completely flat.
            my @parts = map { sprintf("lvl%d", $_) } 1 .. (1 + $d % $nesting);
            my $dir = join("/", $root, sprintf("pkg%04d", $d), @parts);
            make_path($dir);

            for my $f (1 .. $files) {
                my $size = $min + int(rand($max - $min + 1));
                open(my $fh, ">", "$dir/file$f.dat") or die "$dir/file$f.dat: $!";
                # One buffer reused for every file — the content is irrelevant,
                # only the byte count is.
                print $fh "x" x $size;
                close($fh);
            }
        }
    '
}

echo "==> node_modules/: ${NM_PACKAGES} packages x ${NM_FILES_PER_PACKAGE} files (1-8 KiB)"
generate_small_tree "$TARGET/node_modules" "$NM_PACKAGES" "$NM_FILES_PER_PACKAGE" 1024 8192 5

echo "==> cache/: ${CACHE_FILES} files (4 KiB - 2 MiB)"
# 200 directories x 100 files, shallower than node_modules.
generate_small_tree "$TARGET/cache" 200 $((CACHE_FILES / 200)) 4096 2097152 3

echo "==> media/: ${MEDIA_FILES} files x ${MEDIA_MB} MiB, flat"
mkdir -p "$TARGET/media"
for i in $(seq 1 "$MEDIA_FILES"); do
    # `bs=1M` uppercase: GNU dd rejects the lowercase `1m` that BSD dd accepts.
    dd if=/dev/zero of="$TARGET/media/clip$(printf '%02d' "$i").bin" \
        bs=1M count="$MEDIA_MB" status=none
done

echo
echo "fixtures ready in $TARGET"
du -sh "$TARGET"/*
