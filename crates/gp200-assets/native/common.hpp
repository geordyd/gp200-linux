#pragma once
#include <filesystem>
#include <functional>
#include <string>
#include <vector>
#include <cstdint>
namespace ntc {
namespace fs = std::filesystem;
using StatusCallback = std::function<void(const std::wstring&)>;
std::string pathToUtf8(const fs::path&);
bool readFileBytes(const fs::path&, std::vector<std::uint8_t>&, std::string&);
bool writeFileBytes(const fs::path&, const std::uint8_t*, std::size_t, std::string&);
}
