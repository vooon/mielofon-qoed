# The iperf3 server daemon (iperf3-server)
#
# iperf3 needs a listening server on the far end of the probed link; unlike
# netperf (which ships netserver), nothing runs iperf3 -s by default. This
# package provides procd-managed iperf3 servers for the mesh: one instance per
# UCI `server` section, so a node can run a TCP listener on one port and a UDP
# listener on another (the two legs of the gated throughput probe). Each
# server is jailed under ujail by default.

include $(TOPDIR)/rules.mk

PKG_NAME:=iperf3-server
PKG_VERSION:=0.2.0
PKG_RELEASE:=1

PKG_LICENSE:=Apache-2.0
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk

define Package/iperf3-server
  SECTION:=net
  CATEGORY:=Network
  TITLE:=iperf3 server
  URL:=https://github.com/vooon/mielofon-qoed
  PKGARCH:=all
  DEPENDS:=+iperf3 +procd-ujail
endef

define Package/iperf3-server/description
  Runs one iperf3 server per UCI 'server' section (TCP and/or UDP), each
  jailed under ujail. Used as the far-end listener for the mesh throughput
  probe.
endef

define Package/iperf3-server/conffiles
/etc/config/iperf3-server
endef

define Build/Compile
endef

define Package/iperf3-server/install
	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/iperf3-server.conf $(1)/etc/config/iperf3-server

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/iperf3-server.init $(1)/etc/init.d/iperf3-server
endef

$(eval $(call BuildPackage,iperf3-server))
