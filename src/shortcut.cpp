// Windows Search discovers this normal per-user Start menu shortcut.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <shlobj.h>
#include <wrl/client.h>
#include <memory>
#include <new>
#include <string>

using Microsoft::WRL::ComPtr;

namespace {
struct ComApartment {
    HRESULT result = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    ~ComApartment() { if (SUCCEEDED(result)) CoUninitialize(); }
};
struct FreeTaskMemory {
    void operator()(wchar_t* value) const noexcept { CoTaskMemFree(value); }
};
}

extern "C" HRESULT portside_install_shortcut(const wchar_t* executable) noexcept {
    try {
        ComApartment apartment;
        if (FAILED(apartment.result)) return apartment.result;
        wchar_t* raw_programs = nullptr;
        HRESULT hr = SHGetKnownFolderPath(FOLDERID_Programs, KF_FLAG_CREATE, nullptr, &raw_programs);
        std::unique_ptr<wchar_t, FreeTaskMemory> programs(raw_programs);
        if (FAILED(hr)) return hr;
        const std::wstring path = std::wstring(programs.get()) + L"\\Portside.lnk";
        ComPtr<IShellLinkW> link;
        hr = CoCreateInstance(CLSID_ShellLink, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&link));
        if (FAILED(hr)) return hr;
        if (FAILED(hr = link->SetPath(executable))) return hr;
        if (FAILED(hr = link->SetDescription(L"Show local dev servers in the taskbar"))) return hr;
        ComPtr<IPersistFile> file;
        if (FAILED(hr = link.As(&file))) return hr;
        hr = file->Save(path.c_str(), TRUE);
        if (SUCCEEDED(hr)) SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW, path.c_str(), nullptr);
        return hr;
    } catch (const std::bad_alloc&) { return E_OUTOFMEMORY; }
    catch (...) { return E_FAIL; }
}
