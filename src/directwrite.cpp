// Direct2D drawing primitives for the strip and the server menu. Rust owns
// the layout in DIPs; this file only measures and draws. The text font is
// private, loaded from embedded bytes; the icon font comes from Windows.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d2d1.h>
#include <dwrite_3.h>
#include <wrl/client.h>
#include <memory>
#include <new>
#include <string>

using Microsoft::WRL::ComPtr;

namespace {
struct Renderer final {
    ComPtr<ID2D1Factory> d2d;
    ComPtr<IDWriteFactory5> dwrite;
    ComPtr<IDWriteInMemoryFontFileLoader> loader;
    ComPtr<IDWriteFontCollection1> collection;
    bool loader_registered = false;
    std::wstring family, icon_family;
    DWRITE_FONT_WEIGHT weight = DWRITE_FONT_WEIGHT_NORMAL;
    ComPtr<ID2D1DCRenderTarget> target;
    ComPtr<ID2D1SolidColorBrush> brush;

    ~Renderer() noexcept {
        brush.Reset();
        target.Reset();
        collection.Reset();
        if (loader_registered) dwrite->UnregisterFontFileLoader(loader.Get());
    }

    HRESULT initialize(const unsigned char* data, UINT32 length) noexcept {
        HRESULT hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, d2d.GetAddressOf());
        if (FAILED(hr)) return hr;
        hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED, __uuidof(IDWriteFactory5),
            reinterpret_cast<IUnknown**>(dwrite.GetAddressOf()));
        if (FAILED(hr)) return hr;
        hr = dwrite->CreateInMemoryFontFileLoader(loader.GetAddressOf());
        if (FAILED(hr)) return hr;
        hr = dwrite->RegisterFontFileLoader(loader.Get());
        if (FAILED(hr)) return hr;
        loader_registered = true;
        // A null owner makes the loader keep its own copy of the font bytes.
        ComPtr<IDWriteFontFile> file;
        hr = loader->CreateInMemoryFontFileReference(dwrite.Get(), data, length, nullptr,
            file.GetAddressOf());
        if (FAILED(hr)) return hr;
        ComPtr<IDWriteFontSetBuilder1> builder;
        hr = dwrite->CreateFontSetBuilder(builder.GetAddressOf());
        if (FAILED(hr)) return hr;
        hr = builder->AddFontFile(file.Get());
        if (FAILED(hr)) return hr;
        ComPtr<IDWriteFontSet> set;
        hr = builder->CreateFontSet(set.GetAddressOf());
        if (FAILED(hr)) return hr;
        hr = dwrite->CreateFontCollectionFromFontSet(set.Get(), collection.GetAddressOf());
        if (FAILED(hr)) return hr;
        UINT32 index = 0;
        BOOL exists = FALSE;
        hr = collection->FindFamilyName(family.c_str(), &index, &exists);
        return FAILED(hr) ? hr : exists ? S_OK : DWRITE_E_NOFONT;
    }

    // Text is laid out in a box, centered vertically, cut with "…" when wider.
    HRESULT layout(const wchar_t* text, UINT32 length, bool icon, float size, UINT32 feature,
                   float width, float height, int align, IDWriteTextLayout** result) noexcept {
        ComPtr<IDWriteTextFormat> format;
        HRESULT hr = dwrite->CreateTextFormat(icon ? icon_family.c_str() : family.c_str(),
            icon ? nullptr : collection.Get(), weight, DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL, size, L"en-US", format.GetAddressOf());
        if (FAILED(hr)) return hr;
        format->SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        format->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        format->SetTextAlignment(static_cast<DWRITE_TEXT_ALIGNMENT>(align));
        ComPtr<IDWriteInlineObject> ellipsis;
        hr = dwrite->CreateEllipsisTrimmingSign(format.Get(), ellipsis.GetAddressOf());
        if (FAILED(hr)) return hr;
        const DWRITE_TRIMMING trimming{DWRITE_TRIMMING_GRANULARITY_CHARACTER, 0, 0};
        format->SetTrimming(&trimming, ellipsis.Get());
        ComPtr<IDWriteTextLayout> created;
        hr = dwrite->CreateTextLayout(text, length, format.Get(), width, height,
            created.GetAddressOf());
        if (FAILED(hr)) return hr;
        if (feature) {
            ComPtr<IDWriteTypography> typography;
            hr = dwrite->CreateTypography(typography.GetAddressOf());
            if (FAILED(hr)) return hr;
            typography->AddFontFeature({static_cast<DWRITE_FONT_FEATURE_TAG>(feature), 1});
            created->SetTypography(typography.Get(), {0, length});
        }
        *result = created.Detach();
        return S_OK;
    }

    HRESULT begin(HDC dc, int width, int height, float dpi, UINT32 background) noexcept {
        if (!target) {
            const auto properties = D2D1::RenderTargetProperties(D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_IGNORE));
            HRESULT hr = d2d->CreateDCRenderTarget(&properties, target.GetAddressOf());
            if (SUCCEEDED(hr)) hr = target->CreateSolidColorBrush(D2D1::ColorF(0), brush.GetAddressOf());
            if (FAILED(hr)) {
                target.Reset();
                return hr;
            }
            target->SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        }
        target->SetDpi(dpi, dpi);
        const RECT bounds{0, 0, width, height};
        HRESULT hr = target->BindDC(dc, &bounds);
        if (FAILED(hr)) {
            brush.Reset();
            target.Reset();
            return hr;
        }
        target->BeginDraw();
        target->Clear(D2D1::ColorF(background));
        return S_OK;
    }

    ID2D1SolidColorBrush* paint(UINT32 color, float alpha) noexcept {
        brush->SetColor(D2D1::ColorF(color, alpha));
        return brush.Get();
    }
};
} // namespace

extern "C" void* portside_renderer_create(const unsigned char* font, unsigned int length,
    const wchar_t* family, const wchar_t* icon_family, unsigned int weight,
    HRESULT* result) noexcept {
    try {
        auto renderer = std::make_unique<Renderer>();
        renderer->family = family;
        renderer->icon_family = icon_family;
        renderer->weight = static_cast<DWRITE_FONT_WEIGHT>(weight);
        *result = renderer->initialize(font, length);
        return SUCCEEDED(*result) ? renderer.release() : nullptr;
    } catch (const std::bad_alloc&) { *result = E_OUTOFMEMORY; }
    catch (...) { *result = E_FAIL; }
    return nullptr;
}

extern "C" void portside_renderer_destroy(void* renderer) noexcept {
    delete static_cast<Renderer*>(renderer);
}

// The text's width in DIPs, or 0 if it can't be laid out.
extern "C" float portside_measure(void* handle, const wchar_t* text, unsigned int length,
    int icon, float size, unsigned int feature) noexcept {
    ComPtr<IDWriteTextLayout> layout;
    DWRITE_TEXT_METRICS metrics{};
    if (FAILED(static_cast<Renderer*>(handle)->layout(text, length, icon, size, feature,
            1e6f, 1e6f, DWRITE_TEXT_ALIGNMENT_LEADING, layout.GetAddressOf())) ||
        FAILED(layout->GetMetrics(&metrics))) return 0.0f;
    return metrics.widthIncludingTrailingWhitespace;
}

extern "C" HRESULT portside_begin(void* handle, HDC dc, int width, int height, float dpi,
    unsigned int background) noexcept {
    return static_cast<Renderer*>(handle)->begin(dc, width, height, dpi, background);
}

extern "C" void portside_fill(void* handle, const D2D1_RECT_F* rect, float radius,
    unsigned int color, float alpha) noexcept {
    auto* renderer = static_cast<Renderer*>(handle);
    renderer->target->FillRoundedRectangle(D2D1::RoundedRect(*rect, radius, radius),
        renderer->paint(color, alpha));
}

// align: 0 leading, 1 trailing (DWRITE_TEXT_ALIGNMENT).
extern "C" void portside_text(void* handle, const wchar_t* text, unsigned int length, int icon,
    float size, unsigned int feature, const D2D1_RECT_F* rect, int align,
    unsigned int color) noexcept {
    auto* renderer = static_cast<Renderer*>(handle);
    ComPtr<IDWriteTextLayout> layout;
    if (SUCCEEDED(renderer->layout(text, length, icon, size, feature, rect->right - rect->left,
            rect->bottom - rect->top, align, layout.GetAddressOf())))
        renderer->target->DrawTextLayout(D2D1::Point2F(rect->left, rect->top), layout.Get(),
            renderer->paint(color, 1.0f), D2D1_DRAW_TEXT_OPTIONS_NONE);
}

// D2DERR_RECREATE_TARGET drops the target and returns true; the caller repaints.
extern "C" bool portside_end(void* handle) noexcept {
    auto* renderer = static_cast<Renderer*>(handle);
    if (renderer->target->EndDraw() != D2DERR_RECREATE_TARGET) return false;
    renderer->brush.Reset();
    renderer->target.Reset();
    return true;
}
