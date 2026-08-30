include $(TOPDIR)/rules.mk

PKG_NAME:=mielofon-agent
PKG_VERSION:=0.1.0
PKG_RELEASE:=1

PKG_SOURCE_PROTO:=git
PKG_SOURCE_URL:=https://github.com/vooon/mielofon-qoed.git
PKG_MIRROR_HASH:=skip
PKG_SOURCE_VERSION:=v$(PKG_VERSION)

PKG_LICENSE:=Apache-2.0
PKG_LICENSE_FILES:=LICENSE
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

PKG_BUILD_DEPENDS:=rust/host
PKG_BUILD_PARALLEL:=1

include $(INCLUDE_DIR)/package.mk
include ../../packages/lang/rust/rust-package.mk

define Package/mielofon-agent
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Mielofon router agent (probe runner + OSPF cost applifier)
  URL:=https://github.com/vooon/mielofon-qoed
  DEPENDS:=$(RUST_ARCH_DEPENDS)
endef

define Package/mielofon-agent/description
  Mielofon agent: runs on each mesh router, executes the probe and
  apply-cost commands issued by the mielofon-controller and reports raw
  measurements. Thin executor — no scheduling or policy logic of its own.
endef

define Package/mielofon-agent/conffiles
/etc/mielofon/mielofon-agent.toml
endef

define Build/Compile
	cd $(PKG_BUILD_DIR); \
	$(CARGO_PKG_VARS) \
	CC="$(TARGET_CC)" \
	cargo build -v \
		--release \
		--package mielofon-agent
endef

define Package/mielofon-agent/install
	$(INSTALL_DIR) $(1)/usr/sbin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/target/$(RUSTC_TARGET_ARCH)/release/mielofon-agent $(1)/usr/sbin/

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) $(CURDIR)/files/mielofon-agent.init $(1)/etc/init.d/mielofon-agent

	$(INSTALL_DIR) $(1)/etc/mielofon
	$(INSTALL_CONF) $(CURDIR)/files/mielofon-agent.toml $(1)/etc/mielofon/mielofon-agent.toml
endef

$(eval $(call RustBinPackage,mielofon-agent))
$(eval $(call BuildPackage,mielofon-agent))