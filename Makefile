# LanDrop — Makefile
#
# Targets: run, build, test, fmt, vet, clean, install-desktop,
#          uninstall-desktop, install-service, uninstall-service,
#          desktop-dev, desktop-build
#
# Desktop installs are user-local (no sudo) under ~/.local.

BIN       := bin/landrop
CARGO     := cargo

# User-local install prefix (no sudo required).
PREFIX      := $(HOME)/.local
BINDIR      := $(PREFIX)/bin
APPSDIR     := $(PREFIX)/share/applications
ICON32DIR   := $(PREFIX)/share/icons/hicolor/32x32/apps
ICON128DIR  := $(PREFIX)/share/icons/hicolor/128x128/apps
ICON256DIR  := $(PREFIX)/share/icons/hicolor/256x256/apps
ICON512DIR  := $(PREFIX)/share/icons/hicolor/512x512/apps
SVGDIR      := $(PREFIX)/share/icons/hicolor/scalable/apps
# Match `dirs::config_dir()` in the app (respects XDG_CONFIG_HOME).
CONFIG_HOME  := $(if $(XDG_CONFIG_HOME),$(XDG_CONFIG_HOME),$(HOME)/.config)
AUTOSTART    := $(CONFIG_HOME)/autostart
SYSTEMD_USER := $(CONFIG_HOME)/systemd/user

.PHONY: all run build test fmt vet clean icons \
        install-desktop uninstall-desktop \
        install-service uninstall-service \
        desktop-dev desktop-build \
        mobile-targets mobile-init mobile-build mobile-build-phone \
        mobile-sign mobile-install mobile-dev

all: build

run:
	$(CARGO) run

build:
	$(CARGO) build --release
	mkdir -p bin
	cp target/release/landrop $(BIN)
	@echo ""
	@echo "Built $(BIN). Run with: ./$(BIN)"

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt

vet:
	$(CARGO) clippy --all-targets -- -D warnings

clean:
	$(CARGO) clean
	rm -rf bin

# Regenerate icon derivatives (PNG, tray RGBA) from assets/icon.svg.
icons:
	python3 scripts/generate_icons.py

# --- Linux desktop integration (user-local, no sudo) ----------------------

# Install the desktop app (Tauri window) for the current user — no root needed.
# Writes ~/.local/bin/landrop-desktop plus a .desktop entry that shadows the
# distribution package in the application menu.
install-app: icons
	./scripts/install-desktop-app.sh

install-desktop: build icons
	@echo "==> Installing LanDrop to $(PREFIX)"
	mkdir -p $(BINDIR) $(APPSDIR) $(ICON32DIR) $(ICON128DIR) $(ICON256DIR) $(ICON512DIR) $(SVGDIR) $(AUTOSTART)
	cp $(BIN) $(BINDIR)/landrop
	cp assets/landrop.desktop $(APPSDIR)/landrop.desktop
	cp assets/icon.svg $(SVGDIR)/landrop.svg
	cp assets/icon-32.png $(ICON32DIR)/landrop.png
	cp assets/icon-128.png $(ICON128DIR)/landrop.png
	cp assets/icon.png $(ICON256DIR)/landrop.png
	cp assets/icon-512.png $(ICON512DIR)/landrop.png
	sed -i -e "s|^Exec=.*|Exec=$(BINDIR)/landrop|" \
	  -e "s|^StartupWMClass=.*|StartupWMClass=landrop|" \
	  $(APPSDIR)/landrop.desktop
	@# Background receiver on login so other devices can still see this one.
	printf '%s\n' \
	  '[Desktop Entry]' \
	  'Type=Application' \
	  'Name=LanDrop' \
	  'Comment=Background LAN file sharing' \
	  'Exec=$(BINDIR)/landrop --hidden' \
	  'Icon=landrop' \
	  'Terminal=false' \
	  'X-GNOME-Autostart-enabled=true' \
	  'StartupNotify=false' \
	  > $(AUTOSTART)/landrop.desktop
	@# Refresh the desktop database so the app menu picks it up.
	-update-desktop-database $(APPSDIR) 2>/dev/null || true
	-gtk-update-icon-cache -f -t $(PREFIX)/share/icons/hicolor 2>/dev/null || true
	@echo ""
	@echo "Installed. Launch with: landrop  (or find LanDrop in your app menu)"
	@echo "It will also start in the background when you log in."
	@echo "Uninstall with: make uninstall-desktop"

uninstall-desktop:
	@echo "==> Removing LanDrop from $(PREFIX)"
	rm -f $(BINDIR)/landrop
	rm -f $(APPSDIR)/landrop.desktop
	rm -f $(ICON32DIR)/landrop.png
	rm -f $(ICON128DIR)/landrop.png
	rm -f $(ICON256DIR)/landrop.png
	rm -f $(ICON512DIR)/landrop.png
	rm -f $(SVGDIR)/landrop.svg
	rm -f $(AUTOSTART)/landrop.desktop
	-update-desktop-database $(APPSDIR) 2>/dev/null || true
	-gtk-update-icon-cache -f -t $(PREFIX)/share/icons/hicolor 2>/dev/null || true
	@echo "Uninstalled."

# --- systemd user service (background receiver, no login autostart) --------

# Install the binary and a `systemd --user` unit that keeps LanDrop running in
# the background. `--hidden` means no window is opened; the LAN UI still works.
install-service: build
	@echo "==> Installing the LanDrop systemd user unit"
	mkdir -p $(BINDIR) $(SYSTEMD_USER)
	cp $(BIN) $(BINDIR)/landrop
	cp assets/landrop.service $(SYSTEMD_USER)/landrop.service
	-systemctl --user daemon-reload
	-systemctl --user enable --now landrop.service
	@echo ""
	@echo "Enabled. Check it with: systemctl --user status landrop.service"
	@echo "Remove it with: make uninstall-service"

uninstall-service:
	@echo "==> Removing the LanDrop systemd user unit"
	-systemctl --user disable --now landrop.service
	rm -f $(SYSTEMD_USER)/landrop.service
	-systemctl --user daemon-reload
	@echo "Uninstalled."

# --- Android / Tauri mobile -------------------------------------------------
# Needs: Android SDK + NDK (ANDROID_HOME), JDK 17, the Rust android targets
# (make mobile-targets) and the Tauri CLI (cargo install tauri-cli --version '^2').

ANDROID_HOME ?= $(HOME)/Android/Sdk
NDK_VERSION  := $(shell ls -1 $(ANDROID_HOME)/ndk 2>/dev/null | tail -1)
BT_VERSION   := $(shell ls -1 $(ANDROID_HOME)/build-tools 2>/dev/null | tail -1)
JAVA_HOME    ?= $(shell readlink -f $$(command -v java) 2>/dev/null | sed 's|/bin/java$$||')
MOBILE_ENV    = ANDROID_HOME="$(ANDROID_HOME)" NDK_HOME="$(ANDROID_HOME)/ndk/$(NDK_VERSION)" JAVA_HOME="$(JAVA_HOME)"
APK_DIR      := desktop/gen/android/app/build/outputs/apk/universal/release
APK_UNSIGNED := $(APK_DIR)/app-universal-release-unsigned.apk
APK_SIGNED   := $(APK_DIR)/app-universal-release.apk
DEBUG_KEY    := $(HOME)/.android/debug.keystore

mobile-targets:  ## add the Rust android targets
	rustup target add aarch64-linux-android x86_64-linux-android

mobile-init:     ## generate desktop/gen/android (committed on purpose)
	cd desktop && $(MOBILE_ENV) cargo tauri android init

mobile-build: icons    ## APK for the emulator (x86_64)
	cd desktop && $(MOBILE_ENV) cargo tauri android build --apk --target x86_64
	python3 scripts/generate_icons.py --verify-android
	@echo "unsigned: $(APK_UNSIGNED)"

mobile-build-phone: icons  ## APK for real phones (aarch64)
	cd desktop && $(MOBILE_ENV) cargo tauri android build --apk --target aarch64
	python3 scripts/generate_icons.py --verify-android

mobile-sign:     ## sign the last APK with a local debug keystore
	@test -f "$(DEBUG_KEY)" || keytool -genkeypair -keystore "$(DEBUG_KEY)" -storepass android \
	  -keypass android -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
	  -dname "CN=Android Debug,O=Android,C=US"
	"$(ANDROID_HOME)/build-tools/$(BT_VERSION)/zipalign" -p -f 4 "$(APK_UNSIGNED)" "$(APK_UNSIGNED).aligned"
	"$(ANDROID_HOME)/build-tools/$(BT_VERSION)/apksigner" sign --ks "$(DEBUG_KEY)" --ks-pass pass:android \
	  --key-pass pass:android --out "$(APK_SIGNED)" "$(APK_UNSIGNED).aligned"
	@mkdir -p bin
	cp "$(APK_SIGNED)" bin/LanDrop-android-arm64.apk
	@echo "signed: $(APK_SIGNED)"
	@echo "copied: bin/LanDrop-android-arm64.apk"

mobile-install: mobile-sign  ## install on a connected device/emulator
	"$(ANDROID_HOME)/platform-tools/adb" install -r "$(APK_SIGNED)"

mobile-dev:      ## run with hot reload on a device/emulator
	cd desktop && $(MOBILE_ENV) cargo tauri android dev

# --- Tauri desktop wrapper -------------------------------------------------

desktop-dev: icons
	@if command -v cargo-tauri >/dev/null 2>&1; then \
		cd desktop && cargo tauri dev; \
	else \
		echo "cargo-tauri not found. Install with:"; \
		echo "  cargo install tauri-cli --version '^2' --locked"; \
		exit 1; \
	fi

desktop-build: icons
	@if command -v cargo-tauri >/dev/null 2>&1; then \
		cd desktop && cargo tauri build; \
	else \
		echo "cargo-tauri not found. Install with:"; \
		echo "  cargo install tauri-cli --version '^2' --locked"; \
		exit 1; \
	fi
