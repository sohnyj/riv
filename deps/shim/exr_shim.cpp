// Static shim exposing the OpenEXR RgbaInputFile path through extern "C".

#include <ImathBox.h>
#include <ImfIO.h>
#include <ImfRgbaFile.h>
#include <ImfThreading.h>

#include <cstdio>
#include <cstring>
#include <exception>
#include <stdexcept>
#include <thread>

namespace {

// OpenEXR opens ANSI paths only; supply a _wfopen-backed stream for Unicode.
class WideFileStream : public Imf::IStream {
public:
    explicit WideFileStream(const wchar_t* path)
        : Imf::IStream("riv-exr"), file_(_wfopen(path, L"rb")) {}
    WideFileStream(const WideFileStream&) = delete;
    WideFileStream& operator=(const WideFileStream&) = delete;
    ~WideFileStream() override {
        if (file_ != nullptr) {
            std::fclose(file_);
        }
    }

    bool valid() const { return file_ != nullptr; }

    bool read(char buffer[], int count) override {
        if (count < 0 || std::fread(buffer, 1, count, file_) != static_cast<size_t>(count)) {
            throw std::runtime_error("unexpected end of file");
        }
        return std::feof(file_) == 0;
    }

    uint64_t tellg() override { return static_cast<uint64_t>(_ftelli64(file_)); }

    void seekg(uint64_t position) override {
        _fseeki64(file_, static_cast<long long>(position), SEEK_SET);
    }

private:
    std::FILE* file_;
};

// Borrowed in-memory stream for archive members extracted by the Rust side.
class MemoryStream : public Imf::IStream {
public:
    MemoryStream(const unsigned char* data, size_t size)
        : Imf::IStream("riv-exr-memory"), data_(data), size_(size), position_(0) {}

    bool read(char buffer[], int count) override {
        // Subtraction form: a crafted seekg can push position_ past size_.
        const size_t requested = static_cast<size_t>(count);
        if (count < 0 || position_ > size_ || requested > size_ - position_) {
            throw std::runtime_error("unexpected end of data");
        }
        std::memcpy(buffer, data_ + position_, requested);
        position_ += requested;
        return position_ < size_;
    }

    uint64_t tellg() override { return position_; }

    void seekg(uint64_t position) override { position_ = static_cast<size_t>(position); }

private:
    const unsigned char* data_;
    size_t size_;
    size_t position_;
};

void write_error(char* error_message, size_t error_capacity, const char* text) {
    if (error_message != nullptr && error_capacity > 0) {
        std::snprintf(error_message, error_capacity, "%s", text);
    }
}

// Size of the data window with the sanity bound both entry points share.
bool data_window_size(const Imath::Box2i& data_window, long long* out_width,
                      long long* out_height) {
    *out_width = static_cast<long long>(data_window.max.x) - data_window.min.x + 1;
    *out_height = static_cast<long long>(data_window.max.y) - data_window.min.y + 1;
    return *out_width > 0 && *out_height > 0 && *out_width * *out_height <= (1LL << 30);
}

// Reads only the header; the caller computes the decode weight from the size.
int probe_stream(Imf::IStream& stream, int* out_width, int* out_height) {
    try {
        Imf::RgbaInputFile file(stream);
        long long width = 0;
        long long height = 0;
        if (!data_window_size(file.dataWindow(), &width, &height)) {
            return 1;
        }
        *out_width = static_cast<int>(width);
        *out_height = static_cast<int>(height);
        return 0;
    } catch (...) {
        return 1;
    }
}

// Reads into the caller's buffer, which must hold capacity_pixels RGBA halves.
int decode_stream_into(Imf::IStream& stream, unsigned short* out_pixels, size_t capacity_pixels,
                       int* out_width, int* out_height, char* error_message,
                       size_t error_capacity) {
    try {
        static const int thread_count = [] {
            const unsigned int hardware = std::thread::hardware_concurrency();
            return hardware > 0 ? static_cast<int>(hardware) : 2;
        }();
        Imf::setGlobalThreadCount(thread_count);

        Imf::RgbaInputFile file(stream);
        const Imath::Box2i data_window = file.dataWindow();
        long long width = 0;
        long long height = 0;
        if (!data_window_size(data_window, &width, &height)) {
            write_error(error_message, error_capacity, "invalid data window");
            return 1;
        }
        *out_width = static_cast<int>(width);
        *out_height = static_cast<int>(height);
        // The caller sized the buffer from a probe; a file that grew since then stops here.
        if (static_cast<size_t>(width * height) > capacity_pixels) {
            write_error(error_message, error_capacity, "pixel buffer too small");
            return 1;
        }
        auto* pixels = reinterpret_cast<Imf::Rgba*>(out_pixels);
        file.setFrameBuffer(pixels - data_window.min.x
                                - static_cast<long long>(data_window.min.y) * width,
                            1, static_cast<size_t>(width));
        file.readPixels(data_window.min.y, data_window.max.y);
        return 0;
    } catch (const std::exception& exception) {
        write_error(error_message, error_capacity, exception.what());
        return 1;
    } catch (...) {
        write_error(error_message, error_capacity, "unknown OpenEXR error");
        return 1;
    }
}

} // namespace

extern "C" {

// Returns 0 on success, having filled out_pixels; the caller owns that buffer throughout.
int riv_exr_decode_into(const wchar_t* path, unsigned short* out_pixels, size_t capacity_pixels,
                        int* out_width, int* out_height, char* error_message,
                        size_t error_capacity) {
    WideFileStream stream(path);
    if (!stream.valid()) {
        write_error(error_message, error_capacity, "cannot open file");
        return 1;
    }
    return decode_stream_into(stream, out_pixels, capacity_pixels, out_width, out_height,
                              error_message, error_capacity);
}

// In-memory variant; the source buffer is only borrowed for the duration of the call.
int riv_exr_decode_memory_into(const unsigned char* data, size_t size, unsigned short* out_pixels,
                               size_t capacity_pixels, int* out_width, int* out_height,
                               char* error_message, size_t error_capacity) {
    MemoryStream stream(data, size);
    return decode_stream_into(stream, out_pixels, capacity_pixels, out_width, out_height,
                              error_message, error_capacity);
}

// Returns 0 on success with the data window size; no pixels are read.
int riv_exr_probe(const wchar_t* path, int* out_width, int* out_height) {
    WideFileStream stream(path);
    if (!stream.valid()) {
        return 1;
    }
    return probe_stream(stream, out_width, out_height);
}

int riv_exr_probe_memory(const unsigned char* data, size_t size, int* out_width,
                         int* out_height) {
    MemoryStream stream(data, size);
    return probe_stream(stream, out_width, out_height);
}

} // extern "C"
