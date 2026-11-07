# Windows builds: sotf-daemon.exe (cpal/WASAPI capture, no macOS HAL).
# Recipes delegate to scripts/build-windows.ps1; see that file for
# -Configuration Dist / -StaticCrt / -Asio / -NoPackage.

# Build release sotf-daemon.exe and the dist\ zip package.
[group('build-windows')]
[windows]
build-windows:
	powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1

# Build only target\<triple>\release\sotf-daemon.exe (no dist\ package).
[group('build-windows')]
[windows]
build-windows-exe:
	powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1 -NoPackage

# Shipping build: dist profile (fat LTO) with a self-contained static CRT.
[group('build-windows')]
[windows]
dist-windows:
	powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1 -Configuration Dist -StaticCrt
