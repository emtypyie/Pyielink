// dxgi_capture.cpp - GPU-accelerated desktop capture via DXGI Desktop
// Duplication (Windows 8+). Grabs the composited desktop on the GPU and
// reads each frame back as BGRA, writing raw frames to stdout for ffmpeg to
// encode (e.g. with h264_nvenc). This runs at the display refresh rate
// (typically 60/120/144 Hz), unlike gdigrab which is CPU/BitBlt bound.
//
// Usage: dxgi_capture.exe [monitorIndex]
//   stdout: raw BGRA frames, width x height, 4 bytes/pixel
//   stderr: "WIDTH=w HEIGHT=h" on start, plus errors
//
// Build (MinGW-w64):
//   g++.exe -O2 -o dxgi_capture.exe dxgi_capture.cpp -ld3d11 -ldxgi -lole32 -luuid -luser32 -lgdi32

#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <objbase.h>
#include <stdio.h>
#include <stdint.h>
#include <signal.h>
#include <io.h>
#include <fcntl.h>
#include <chrono>
#include <cstring>
#include <cstdlib>
#include <vector>

static ID3D11Device* g_dev = nullptr;
static ID3D11DeviceContext* g_ctx = nullptr;
static IDXGIOutputDuplication* g_dup = nullptr;
static ID3D11Texture2D* g_staging = nullptr;
static UINT g_w = 0, g_h = 0;
static volatile int g_running = 1;
static std::vector<char> g_outBuf;   // contiguous CPU frame buffer

// Hardware cursor is NOT part of the DXGI desktop frame; composite it ourselves.
static BYTE* g_ptrShape = nullptr;
static UINT g_ptrSize = 0, g_ptrType = 0, g_ptrW = 0, g_ptrH = 0, g_ptrPitch = 0, g_ptrHotX = 0, g_ptrHotY = 0;
static bool g_ptrValid = false;

static void freePtr() {
    if (g_ptrShape) { free(g_ptrShape); g_ptrShape = nullptr; }
    g_ptrSize = 0; g_ptrValid = false;
}

// Best-effort pointer compositing. COLOR (type 2) cursors use straight alpha
// blend; legacy MASKED/MONOCHROME cursors fall back to an XOR of the color
// plane (rare on modern Windows, but never crashes).
static void drawPointer(BYTE* buf, int W, int H, int px, int py) {
    if (!g_ptrValid || !g_ptrShape) return;
    int sw = (int)g_ptrW, sh = (int)g_ptrH;
    for (int y = 0; y < sh; y++) {
        int dy = py + y; if (dy < 0 || dy >= H) continue;
        for (int x = 0; x < sw; x++) {
            int dx = px + x; if (dx < 0 || dx >= W) continue;
            const BYTE* s = g_ptrShape + (size_t)y * g_ptrPitch + (size_t)x * 4;
            BYTE sb = s[0], sg = s[1], sr = s[2], sa = s[3];
            BYTE* d = buf + ((size_t)dy * (size_t)W + (size_t)dx) * 4;
            if (g_ptrType == 2) {
                float a = sa / 255.0f;
                d[0] = (BYTE)(sb * a + d[0] * (1 - a));
                d[1] = (BYTE)(sg * a + d[1] * (1 - a));
                d[2] = (BYTE)(sr * a + d[2] * (1 - a));
                d[3] = 255;
            } else {
                d[0] ^= sb; d[1] ^= sg; d[2] ^= sr; d[3] = 255;
            }
        }
    }
}

static void cleanup() {
    if (g_staging) { g_staging->Release(); g_staging = nullptr; }
    if (g_dup) { g_dup->Release(); g_dup = nullptr; }
    if (g_ctx) { g_ctx->Release(); g_ctx = nullptr; }
    if (g_dev) { g_dev->Release(); g_dev = nullptr; }
    freePtr();
}

static int init(int monitorIndex) {
    cleanup();
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
        &g_dev, nullptr, &g_ctx);
    if (FAILED(hr)) { fprintf(stderr, "D3D11CreateDevice failed 0x%08x\n", (unsigned)hr); return -1; }

    IDXGIDevice* dxgiDev = nullptr;
    g_dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgiDev);
    IDXGIAdapter* adapter = nullptr;
    dxgiDev->GetAdapter(&adapter);
    dxgiDev->Release();

    IDXGIOutput* out = nullptr;
    int idx = 0;
    HRESULT hOut = S_OK;
    while ((hOut = adapter->EnumOutputs(idx, &out)) != DXGI_ERROR_NOT_FOUND) {
        if (idx == monitorIndex) break;
        if (out) { out->Release(); out = nullptr; }
        idx++;
    }
    adapter->Release();
    if (!out) { fprintf(stderr, "monitor %d not found\n", monitorIndex); cleanup(); return -1; }

    DXGI_OUTPUT_DESC odesc;
    out->GetDesc(&odesc);
    g_w = odesc.DesktopCoordinates.right - odesc.DesktopCoordinates.left;
    g_h = odesc.DesktopCoordinates.bottom - odesc.DesktopCoordinates.top;

    IDXGIOutput1* out1 = nullptr;
    out->QueryInterface(__uuidof(IDXGIOutput1), (void**)&out1);
    out->Release();
    hr = out1->DuplicateOutput(g_dev, &g_dup);
    out1->Release();
    if (FAILED(hr)) { fprintf(stderr, "DuplicateOutput failed 0x%08x\n", (unsigned)hr); cleanup(); return -1; }

    D3D11_TEXTURE2D_DESC td;
    memset(&td, 0, sizeof(td));
    td.Width = g_w; td.Height = g_h; td.MipLevels = 1; td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_STAGING;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    hr = g_dev->CreateTexture2D(&td, nullptr, &g_staging);
    if (FAILED(hr)) { fprintf(stderr, "CreateTexture2D(staging) failed 0x%08x\n", (unsigned)hr); cleanup(); return -1; }
    g_outBuf.resize((size_t)g_w * g_h * 4);

    fprintf(stderr, "WIDTH=%u HEIGHT=%u\n", g_w, g_h);
    fflush(stderr);
    return 0;
}

int main(int argc, char** argv) {
    int monitorIndex = (argc > 1) ? atoi(argv[1]) : 0;
    int probeMode = 0;
    for (int i = 1; i < argc; i++) { if (strcmp(argv[i], "--probe") == 0) probeMode = 1; }
    if (FAILED(CoInitializeEx(nullptr, COINIT_MULTITHREADED))) {
        fprintf(stderr, "CoInitializeEx failed\n");
    }
    _setmode(_fileno(stdout), _O_BINARY);

    signal(SIGINT, [](int) { g_running = 0; });
    signal(SIGTERM, [](int) { g_running = 0; });

    if (init(monitorIndex) != 0) { CoUninitialize(); return 1; }
    if (probeMode) {
        fprintf(stdout, "WIDTH=%u HEIGHT=%u\n", g_w, g_h);
        fflush(stdout);
        cleanup();
        CoUninitialize();
        return 0;
    }

    DXGI_OUTDUPL_FRAME_INFO info;
    IDXGIResource* frame = nullptr;
    int perfCnt = 0; double perfAq = 0, perfCw = 0, perfLastCopy = 0, perfLastWrite = 0;
    while (g_running) {
        auto t0 = std::chrono::steady_clock::now();
        HRESULT hr = g_dup->AcquireNextFrame(1000, &info, &frame);
        auto t1 = std::chrono::steady_clock::now();
        if (hr == DXGI_ERROR_WAIT_TIMEOUT) continue;
        if (hr == DXGI_ERROR_ACCESS_LOST) {
            fprintf(stderr, "access lost, reinit\n"); fflush(stderr);
            if (init(monitorIndex) != 0) break;
            continue;
        }
        if (FAILED(hr)) { fprintf(stderr, "AcquireNextFrame failed 0x%08x\n", (unsigned)hr); break; }

        if (info.PointerShapeBufferSize > 0) {
            if (info.PointerShapeBufferSize > g_ptrSize) {
                free(g_ptrShape);
                g_ptrShape = (BYTE*)malloc(info.PointerShapeBufferSize);
                g_ptrSize = info.PointerShapeBufferSize;
            }
            UINT reqSize = 0;
            DXGI_OUTDUPL_POINTER_SHAPE_INFO psi;
            HRESULT hr2 = g_dup->GetFramePointerShape(info.PointerShapeBufferSize, g_ptrShape, &reqSize, &psi);
            if (SUCCEEDED(hr2)) {
                g_ptrType = psi.Type; g_ptrW = psi.Width; g_ptrH = psi.Height;
                g_ptrPitch = psi.Pitch; g_ptrHotX = psi.HotSpot.x; g_ptrHotY = psi.HotSpot.y; g_ptrValid = true;
            }
        }

        ID3D11Texture2D* tex = nullptr;
        frame->QueryInterface(__uuidof(ID3D11Texture2D), (void**)&tex);
        auto tc0 = std::chrono::steady_clock::now();
        g_ctx->CopyResource(g_staging, tex);
        auto tc1 = std::chrono::steady_clock::now();
        D3D11_MAPPED_SUBRESOURCE m;
        if (SUCCEEDED(g_ctx->Map(g_staging, 0, D3D11_MAP_READ, 0, &m))) {
            const UINT rowBytes = g_w * 4;
            auto tw0 = std::chrono::steady_clock::now();
            char* dst = g_outBuf.data();
            const char* src = (const char*)m.pData;
            for (UINT y = 0; y < g_h; y++) {
                memcpy(dst + (size_t)y * rowBytes, src + (size_t)y * m.RowPitch, rowBytes);
            }
            auto tw1 = std::chrono::steady_clock::now();
            if (info.PointerPosition.Visible && g_ptrValid) {
                drawPointer((BYTE*)dst, (int)g_w, (int)g_h,
                    (int)info.PointerPosition.Position.x - (int)g_ptrHotX,
                    (int)info.PointerPosition.Position.y - (int)g_ptrHotY);
            }
            fwrite(dst, 1, (size_t)g_h * rowBytes, stdout);
            auto tw2 = std::chrono::steady_clock::now();
            g_ctx->Unmap(g_staging, 0);
            fflush(stdout);
            perfLastCopy = std::chrono::duration<double, std::milli>(tw1 - tw0).count();
            perfLastWrite = std::chrono::duration<double, std::milli>(tw2 - tw1).count();
        }
        auto t2 = std::chrono::steady_clock::now();
        if (tex) tex->Release();
        frame->Release();
        g_dup->ReleaseFrame();

        perfCnt++;
        perfAq += std::chrono::duration<double, std::milli>(t1 - t0).count();
        perfCw += std::chrono::duration<double, std::milli>(t2 - t1).count();
        if (perfCnt >= 10) {
            if (getenv("PYIELINK_DXGI_PERF")) {
                fprintf(stderr, "[perf] acquireWait=%.1fms  copy+write=%.1fms  (copy=%.1f write=%.1f)\n",
                    perfAq / perfCnt, perfCw / perfCnt, perfLastCopy, perfLastWrite);
                fflush(stderr);
            }
            perfCnt = 0; perfAq = 0; perfCw = 0;
        }
    }

    cleanup();
    CoUninitialize();
    return 0;
}
