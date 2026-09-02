# The iperf3 server daemon (iperf3-server)
#
# iperf3 needs a listening server on the far end of the probed link; unlike
# netperf (which ships netserver), nothing runs iperf3 -s by default. This
# tiny package provides a procd-managed iperf3 server for the mesh. The OSPF
# cost/probe port is configurable via UCI so it can sit away from 5201.

include $(TOPDIR)/rules.mk

PKG_NAME:=iperf3-server
PKG_VERSION:=0.1.0
PKG_RELEASE:=1

PKG_LICENSE:=Apache-2.0
PKG_MAINTAINER:=Vladimir Ermakov <vooon341@gmail.com>

include $(INCLUDE_DIR)/package.mk

define Package/iperf3-server
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Routing and Redirection
  TITLE:=Mielofon iperf3 server (mesh throughput probes)
  URL:=https://github.com/vooon/qoed
  PKGARCH:=all
  DEPENDS:=+iperf3
endef

define Package/iperf3-server/description
  Runs iperf3 in server mode.
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
