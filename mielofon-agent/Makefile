include $(TOPDIR)/rules.mk

PKG_NAME:=mielofon-agent
PKG_VERSION:=0.1.0
PKG_RELEASE:=1

PKG_LICENSE:=Apache-2.0
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk

define Package/mielofon-agent
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Mielofon router agent (ucode probe runner + OSPF cost applifier)
  URL:=https://github.com/vooon/mielofon-qoed
  PKGARCH:=all
  DEPENDS:=+ucode +ucode-mod-uclient +ucode-mod-ubus +ucode-mod-uci +ucode-mod-fs \
	+ucode-mod-uloop +ucode-mod-log +rpcd +rpcd-mod-bird \
	+iperf3 +netperf +iputils-ping \
	+PACKAGE_libustream-openssl:libustream-openssl \
	+PACKAGE_libustream-mbedtls:libustream-mbedtls
endef

define Package/mielofon-agent/description
  Mielofon agent: runs on each mesh router, pulls commands from the
  mielofon-controller over an mTLS long-poll, executes the probe it is told to
  run (or applies the OSPF cost it is told to apply), and reports back
  correlating each reply with the job id. Thin executor — no scheduling or
  policy logic of its own.

  Requires ucode-mod-uclient to be built WITH SSL support (the mielofon-agent
  config block guarantees a libustream-* backend is always enabled). BIRD is
  driven over ubus via rpcd-mod-bird.
endef

define Package/mielofon-agent/config
	if PACKAGE_mielofon-agent

		# Guarantee uclient has a TLS backend at runtime: openssl when the
		# image chose it, otherwise mbedtls as the fallback, so an SSL-less
		# ucode-mod-uclient build can never slip through. (The select guard
		# must NOT reference the target symbol — kconfig flags a self-select
		# as a recursive dependency.)
		config MIELOFON_AGENT_TLS
			bool
			default y
			select PACKAGE_libustream-mbedtls if !PACKAGE_libustream-openssl

	endif
endef

define Package/mielofon-agent/conffiles
/etc/config/mielofon-agent
/etc/mielofon/
endef

define Build/Compile
endef

define Package/mielofon-agent/install
	$(INSTALL_DIR) $(1)/etc/mielofon
	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/etc/config/mielofon-agent $(1)/etc/config/

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/etc/init.d/mielofon-agent $(1)/etc/init.d/

	$(INSTALL_DIR) $(1)/usr/share/ucode/mielofon
	$(INSTALL_DATA) \
		./files/usr/share/ucode/mielofon/mielofon-agent.uc \
		./files/usr/share/ucode/mielofon/client.uc \
		./files/usr/share/ucode/mielofon/cost.uc \
		./files/usr/share/ucode/mielofon/metrics.uc \
		./files/usr/share/ucode/mielofon/probes.uc \
		./files/usr/share/ucode/mielofon/transport.uc \
		./files/usr/share/ucode/mielofon/utils.uc \
		$(1)/usr/share/ucode/mielofon/
endef

$(eval $(call BuildPackage,mielofon-agent))
