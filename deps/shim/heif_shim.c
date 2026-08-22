// Compiled against the synced libheif header, so the versioned options layout never crosses to Rust.

#include <libheif/heif.h>

// Without this flag a repeat-marked edit list makes the sequence decode endless; riv loops on its own.
struct heif_decoding_options* riv_heif_sequence_decoding_options_alloc(void) {
    struct heif_decoding_options* options = heif_decoding_options_alloc();
    if (options != NULL) {
        options->ignore_sequence_editlist = 1;
    }
    return options;
}
