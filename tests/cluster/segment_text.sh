# Source this: seg_text FILE... prints the bytes of raw archive segments as
# stored before compression, so tests can grep them for record markers.
#
# Segments written after v2.19.2 are an 8-byte "CHRNKZ1\n" header followed by
# a zstd frame; earlier ones are plain. Grepping a compressed file finds
# nothing, which reads exactly like "the record is not archived".
# WAL files (.log) are passed through unchanged.
seg_text() {
  local f
  for f in "$@"; do
    [ -f "$f" ] || continue
    if [ "$(head -c 8 "$f")" = "CHRNKZ1" ]; then
      tail -c +9 "$f" | zstd -dcq 2>/dev/null
    else
      cat "$f"
    fi
  done
}
