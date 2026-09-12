include $(TOPDIR)/rules.mk

PKG_NAME:=mielofon-probe
PKG_VERSION:=0.10.1
PKG_RELEASE:=1

PKG_BUILD_DIR:=$(BUILD_DIR)/mielofon-probe-$(PKG_VERSION)
PKG_BUILD_PARALLEL:=1

CMAKE_INSTALL:=1
CMAKE_BINARY_SUBDIR:=build-cmake

PKG_LICENSE:=Apache-2.0
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk
include $(INCLUDE_DIR)/cmake.mk

# Root CMakeLists.txt + C++ sources live at the feed-root/package dir; the
# OpenWrt build pulls them into the build dir. Deps come from the target
# toolchain + packages feed, never from the daemon exec'ing binaries.
define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	$(CP) ./CMakeLists.txt $(PKG_BUILD_DIR)/
	$(CP) -r ./src $(PKG_BUILD_DIR)/
endef

CMAKE_OPTIONS += \
	-DMIELOFON_PROBE_OS=openwrt \
	-DMIELOFON_PROBE_ARCH=$(ARCH) \
	-DMIELOFON_PROBE_VERSION=$(PKG_VERSION) \
	-DCMAKE_CXX_FLAGS_RELEASE=""

define Package/mielofon-probe
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Mielofon measurement probe daemon (C++23)
  URL:=https://github.com/vooon/mielofon-qoed
  DEPENDS:=+libubus +libubox +libjson-c +libiperf3 +libstdcpp +libatomic
endef

define Package/mielofon-probe/description
  Resident measurement daemon for mielofon: continuous ICMP latency/jitter/
  loss per configured link, interface-counter utilisation, gated libiperf3 TCP+
  UDP throughput. The ucode agent configures it over ubus and harvests results;
  it also writes a Prometheus textfile. No config-file parsing of its own.
endef

define Package/mielofon-probe/conffiles
/etc/config/mielofon-probe
/etc/capabilities/mielofon-probe.json
endef

define Package/mielofon-probe/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_INSTALL_DIR)/usr/bin/mielofon-probe $(1)/usr/bin/

	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/mielofon-probe.conf $(1)/etc/config/mielofon-probe

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/mielofon-probe.init $(1)/etc/init.d/mielofon-probe

	$(INSTALL_DIR) $(1)/etc/capabilities
	$(INSTALL_DATA) ./files/mielofon-probe.json $(1)/etc/capabilities/mielofon-probe.json
endef

$(eval $(call BuildPackage,mielofon-probe))
