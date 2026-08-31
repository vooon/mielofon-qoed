include $(TOPDIR)/rules.mk

PKG_NAME:=mielofon-controller
PKG_VERSION:=0.3.8
PKG_RELEASE:=1

PKG_BUILD_DIR:=$(BUILD_DIR)/mielofon-controller-$(PKG_VERSION)
PKG_BUILD_DEPENDS:=rust/host
PKG_BUILD_PARALLEL:=1

PKG_LICENSE:=Apache-2.0
PKG_LICENSE_FILES:=LICENSE
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk
include ../../packages/lang/rust/rust-package.mk

define Package/mielofon-controller
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Distributed QoE link-quality coordination controller
  URL:=https://github.com/vooon/mielofon-qoed
  DEPENDS:=$(RUST_ARCH_DEPENDS) +ca-bundle
endef

define Package/mielofon-controller/description
  Mielofon controller: a small Rust daemon forming a multi-node cluster that
  keeps a replicated per-link quality store, a probe mutual-exclusion fence, an
  mTLS HTTP API, an OTLP/Prometheus observability surface, and an embedded
  dashboard. It is the decision-maker for the whole mesh.
endef

define Package/mielofon-controller/conffiles
/etc/config/mielofon-controller
/etc/mielofon/mielofon-controller.toml
/etc/mielofon/
endef

# This repo is both the source and the feed; build from the working tree
# (no PKG_SOURCE download). Copy the Cargo workspace into the build dir.
define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	$(CP) $(CURDIR)/../Cargo.toml $(PKG_BUILD_DIR)/
	$(CP) $(CURDIR)/../Cargo.lock $(PKG_BUILD_DIR)/
	$(CP) $(CURDIR)/../crates $(PKG_BUILD_DIR)/
endef

define Build/Compile
	cd $(PKG_BUILD_DIR); \
	$(CARGO_PKG_VARS) \
	CC="$(TARGET_CC)" \
	cargo build -v \
		--release \
		--package mielofon-controller
endef

define Package/mielofon-controller/install
	$(INSTALL_DIR) $(1)/usr/sbin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/target/$(RUSTC_TARGET_ARCH)/release/mielofon-controller $(1)/usr/sbin/

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) $(CURDIR)/files/mielofon-controller.init $(1)/etc/init.d/mielofon-controller

	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) $(CURDIR)/files/mielofon-controller.conf $(1)/etc/config/mielofon-controller

	$(INSTALL_DIR) $(1)/etc/mielofon
	$(INSTALL_CONF) $(CURDIR)/files/mielofon-controller.toml $(1)/etc/mielofon/mielofon-controller.toml
endef

$(eval $(call RustBinPackage,mielofon-controller))
$(eval $(call BuildPackage,mielofon-controller))
