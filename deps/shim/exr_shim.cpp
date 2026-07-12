// Static shim exposing the OpenEXR RgbaInputFile path through extern "C".
// Output stays raw RGBA half; tone mapping happens on the Rust side.

#include <ImathBox.h>
#include <ImfIO.h>
#include <ImfRgbaFile.h>
#include <ImfThreading.h>

#include <cstdio>
#include <cstdlib>
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
        if (std::fread(buffer, 1, count, file_) != static_cast<size_t>(count)) {
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
        if (count < 0 || position_ + static_cast<size_t>(count) > size_) {
            throw std::runtime_error("unexpected end of data");
        }
        std::memcpy(buffer, data_ + position_, static_cast<size_t>(count));
        position_ += static_cast<size_t>(count);
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

int decode_stream(Imf::IStream& stream, int* out_width, int* out_height,
                  unsigned short** out_pixels, char* error_message, size_t error_capacity) {
    try {
        static const int thread_count = [] {
            const unsigned int hardware = std::thread::hardware_concurrency();
            return hardware > 0 ? static_cast<int>(hardware) : 2;
        }();
        Imf::setGlobalThreadCount(thread_count);

        Imf::RgbaInputFile file(stream);
        const Imath::Box2i data_window = file.dataWindow();
        const long long width = static_cast<long long>(data_window.max.x) - data_window.min.x + 1;
        const long long height = static_cast<long long>(data_window.max.y) - data_window.min.y + 1;
        if (width <= 0 || height <= 0 || width * height > (1LL << 30)) {
            write_error(error_message, error_capacity, "invalid data window");
            return 1;
        }
        auto* pixels = static_cast<Imf::Rgba*>(
            std::malloc(static_cast<size_t>(width * height) * sizeof(Imf::Rgba)));
        if (pixels == nullptr) {
            write_error(error_message, error_capacity, "pixel buffer allocation failed");
            return 1;
        }
        file.setFrameBuffer(pixels - data_window.min.x
                                - static_cast<long long>(data_window.min.y) * width,
                            1, static_cast<size_t>(width));
        try {
            file.readPixels(data_window.min.y, data_window.max.y);
        } catch (...) {
            std::free(pixels);
            throw;
        }
        *out_width = static_cast<int>(width);
        *out_height = static_cast<int>(height);
        *out_pixels = reinterpret_cast<unsigned short*>(pixels);
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

// Returns 0 on success; out_pixels is a malloc'd RGBA half buffer freed by riv_exr_free.
int riv_exr_decode(const wchar_t* path, int* out_width, int* out_height,
                   unsigned short** out_pixels, char* error_message, size_t error_capacity) {
    WideFileStream stream(path);
    if (!stream.valid()) {
        write_error(error_message, error_capacity, "cannot open file");
        return 1;
    }
    return decode_stream(stream, out_width, out_height, out_pixels, error_message,
                         error_capacity);
}

// In-memory variant; the buffer is only borrowed for the duration of the call.
int riv_exr_decode_memory(const unsigned char* data, size_t size, int* out_width,
                          int* out_height, unsigned short** out_pixels, char* error_message,
                          size_t error_capacity) {
    MemoryStream stream(data, size);
    return decode_stream(stream, out_width, out_height, out_pixels, error_message,
                         error_capacity);
}

void riv_exr_free(unsigned short* pixels) {
    std::free(pixels);
}

} // extern "C"
