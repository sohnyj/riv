// OpenEXR C++ API를 extern "C" 표면으로 감싸는 정적 심 (PORTING_PLAN §5 — EXR fallback).
// RgbaInputFile 경로 = 채널 정규화(YC → RGB 포함) + IlmThread 전역 풀 멀티스레드 디코드.
// 출력은 RGBA half 원본 그대로 — 톤 다운(HDR→SDR)은 Rust 어댑터가 수행한다 (SPEC §10).

#include <ImathBox.h>
#include <ImfIO.h>
#include <ImfRgbaFile.h>
#include <ImfThreading.h>

#include <cstdio>
#include <cstdlib>
#include <exception>
#include <stdexcept>
#include <thread>

namespace {

// 유니코드 경로 지원 — OpenEXR 기본 파일 열기는 ANSI 경로라 _wfopen 스트림을 공급한다
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

void write_error(char* error_message, size_t error_capacity, const char* text) {
    if (error_message != nullptr && error_capacity > 0) {
        std::snprintf(error_message, error_capacity, "%s", text);
    }
}

} // namespace

extern "C" {

// 성공 시 0. out_pixels = dataWindow 크기의 RGBA half 버퍼(r,g,b,a 순, malloc) —
// riv_exr_free로 해제한다. 실패 시 error_message에 사유를 남기고 0이 아닌 값 반환.
int riv_exr_decode(const wchar_t* path, int* out_width, int* out_height,
                   unsigned short** out_pixels, char* error_message, size_t error_capacity) {
    try {
        static const int thread_count = [] {
            const unsigned int hardware = std::thread::hardware_concurrency();
            return hardware > 0 ? static_cast<int>(hardware) : 2;
        }();
        Imf::setGlobalThreadCount(thread_count);

        WideFileStream stream(path);
        if (!stream.valid()) {
            write_error(error_message, error_capacity, "cannot open file");
            return 1;
        }
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

void riv_exr_free(unsigned short* pixels) {
    std::free(pixels);
}

} // extern "C"
