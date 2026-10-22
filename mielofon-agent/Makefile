include $(TOPDIR)/rules.mk

PKG_NAME:=mielofon-agent
PKG_VERSION:=0.16.0
PKG_RELEASE:=1

PKG_BUILD_DIR:=$(BUILD_DIR)/mielofon-agent-$(PKG_VERSION)
PKG_BUILD_DEPENDS:=rust/host
PKG_BUILD_PARALLEL:=1

PKG_LICENSE:=Apache-2.0
PKG_LICENSE_FILES:=LICENSE
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk
include ../../packages/lang/rust/rust-package.mk

define Package/mielofon-agent
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Mielofon node agent (link probing + OSPF cost execution)
  URL:=https://github.com/vooon/mielofon-qoed
  DEPENDS:=$(RUST_ARCH_DEPENDS) +ubus +rpcd +rpcd-mod-bird +iperf3 +iperf3-server
endef

define Package/mielofon-agent/description
  Mielofon agent: runs on each mesh router, discovers its tunnels from
  BIRD/OSPF (rpcd-mod-bird), pings them continuously, runs gated iperf3
  throughput tests, applies the OSPF costs the mielofon-controller decides,
  and answers its trace lookups. Holds an mTLS QUIC session with a
  controller and answers each command stream on it. Thin executor — no
  scheduling or policy logic of its own.
endef

define Package/mielofon-agent/conffiles
/etc/config/mielofon-agent
/etc/mielofon/
endef

# This repo is both the source and the feed; build from the working tree.
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
		--package mielofon-agent
endef

define Package/mielofon-agent/install
	$(INSTALL_DIR) $(1)/usr/sbin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/target/$(RUSTC_TARGET_ARCH)/release/mielofon-agent $(1)/usr/sbin/

	$(INSTALL_DIR) $(1)/etc/mielofon
	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/etc/config/mielofon-agent $(1)/etc/config/

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/etc/init.d/mielofon-agent $(1)/etc/init.d/
endef

$(eval $(call RustBinPackage,mielofon-agent))
$(eval $(call BuildPackage,mielofon-agent))
