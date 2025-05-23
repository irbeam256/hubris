// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{Phy, PhyRw, Trace, VscError};
use ringbuf::ringbuf_entry_root as ringbuf_entry;
use userlib::hl::sleep_for;
use vsc7448_pac::phy;

pub const VSC8504_ID: u32 = 0x704c2;

/// Represents a VSC8504, which is a 4-port PHY used on Sidecar.
/// `base_port` is the PHY address of the chip's port 0.
pub struct Vsc8504 {
    base_port: u8,
}

impl Vsc8504 {
    /// Constructs an invalid Vsc8504, which will panic if you call the
    /// `phy()` function.
    pub fn empty() -> Self {
        Self { base_port: 0xFF }
    }

    /// Initializes a VSC8504 PHY in QSGMII MAC-to-Protocol transfer mode, based
    /// on the "Configuration" guide in the datasheet (section 3.19).  This
    /// should be called _after_ the PHY is reset (i.e. the reset pin is toggled
    /// and then the caller waits for 120 ms).  The caller is also responsible
    /// for handling the `COMA_MODE` pin.
    ///
    /// This must be called on the base port of the PHY, and will configure all
    /// ports using broadcast writes.
    pub fn init_qsgmii_protocol_xfer<P: PhyRw>(
        base_port: u8,
        rw: &mut P,
    ) -> Result<Self, VscError> {
        let out = Self { base_port };
        out.phy(0, rw).init_qsgmii_protocol_xfer()?;

        Ok(out)
    }

    /// Initializes a VSC8504 PHY using QSGMII in MAC-to-Cat5 Link Partner mode
    ///
    /// This must be called on the base port of the PHY, and will configure all
    /// ports using broadcast writes.
    pub fn init_qsgmii_to_cat5<P: PhyRw>(
        base_port: u8,
        rw: &mut P,
    ) -> Result<Self, VscError> {
        let out = Self { base_port };
        out.phy(0, rw).init_qsgmii_to_cat5()?;

        Ok(out)
    }

    /// Returns a handle to address the specified port, which must be in the
    /// range 0-3; this function offsets by the chip's port offset, which is
    /// set by resistor strapping and stored in `self.base_port`.
    pub fn phy<'a, P: PhyRw>(
        &self,
        port: u8,
        rw: &'a mut P,
    ) -> Vsc8504Phy<'a, P> {
        assert!(port < 4);
        assert!(self.base_port != 0xFF);
        Vsc8504Phy {
            phy: Phy::new(self.base_port + port, rw),
        }
    }

    /// Sets the SIGDET polarity for all PHYs (by default, active high)
    pub fn set_sigdet_polarity<P: PhyRw>(
        &self,
        rw: &mut P,
        active_low: bool,
    ) -> Result<(), VscError> {
        // TODO: this is the same code as VSC85x2; should we consolidate?
        self.phy(0, rw).phy.broadcast(|phy| {
            phy.modify(phy::EXTENDED::EXTENDED_MODE_CONTROL(), |r| {
                // TODO: fix VSC7448 codegen to include `sigdet_polarity` bit
                let mut v = u16::from(*r);
                v = (v & !1) | active_low as u16;
                *r = v.into();
            })
        })
    }
}

////////////////////////////////////////////////////////////////////////////////

pub struct Vsc8504Phy<'a, P> {
    pub phy: Phy<'a, P>,
}

impl<'a, P: PhyRw> Vsc8504Phy<'a, P> {
    /// Configure the VSC8504 in protocol transfer (QSGMII to SGMII) mode,
    /// based on section 3.3.3 of the datasheet, ENT-AN1175, and
    /// `vtss_phy_pass_through_speed_mode` in the SDK.
    fn init_qsgmii_protocol_xfer(&mut self) -> Result<(), VscError> {
        ringbuf_entry!(Trace::Vsc8504Init(self.phy.port));

        let id = self.phy.read_id()?;
        if id != VSC8504_ID {
            return Err(VscError::BadPhyId(id));
        }

        let rev = self.phy.read(phy::GPIO::EXTENDED_REVISION())?;
        if rev.tesla_e() != 1 {
            return Err(VscError::BadPhyRev);
        }

        let phy_port = self.phy.get_port()?;
        let is_base_port = phy_port == 0;

        // `vtss_phy_pre_init_seq_tesla`, which calls
        // `vtss_phy_pre_init_seq_tesla_rev_e` (since we check above for rev E)
        if is_base_port {
            crate::tesla::TeslaPhy { phy: &mut self.phy }.patch()?;
        }

        // This is a TESLA PHY.  Here, we roughly follow the SDK function
        // `vtss_phy_reset_private`.  Please forgive the overlap with
        // vsc8562.rs; I'm doing my best to split into PHY-specific functions
        // instead of having MEGA-FUNCTIONS to handle every single PHY.

        // "-- Step 2: Pre-reset setup of MAC and Media interface --"
        // (There is no Step 1, apparently)
        //
        // We are now entering
        //      phy_reset_private
        //          vtss_phy_mac_media_if_tesla_setup

        // "Setup MAC Configuration" (5760)
        // (this is a global register, so we only need to write to it once, but
        // the SDK writes to it for each PHY, so we'll do the same)
        self.phy.modify(phy::GPIO::MAC_MODE_AND_FAST_LINK(), |r| {
            r.0 = (r.0 & !0x6000) | 0x4000
        })?;

        // "Configure SerDes macros for QSGMII MAC interface (See TN1080)"
        // (line 5836)
        if is_base_port {
            self.phy.cmd(0x80E0)?;
        }
        sleep_for(10); // (line 5928)

        // We are running with fiber media, so
        // "Setup media in micro program" (5946)
        self.phy.cmd(0x80C1 | (0x0100 << phy_port))?;
        sleep_for(10);

        // "Setup Media interface" (5952)
        self.phy
            .modify(phy::STANDARD::EXTENDED_PHY_CONTROL(), |r| {
                // SerDes fiber/SFP protocol transfer mode only
                r.set_media_operating_mode(0b001);
            })?;

        // "Set packet mode" (line 5961)
        // Skipping this for now.

        // Congratulations!
        // You are now exiting vtss_phy_mac_media_if_tesla_setup and returning
        // to phy_reset_private:8313

        // "-- Step 3: Reset PHY --" (line 8349)
        // We are now entering
        //      phy_reset_private
        //          port_reset
        //              vtss_atom_patch_suspend(..., true)
        crate::atom::atom_patch_suspend(&mut self.phy)?;

        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //
        // The soft reset for the TESLA PHY is different, for some reason!
        // "Tesla PHY Only - Writing 0xc040, See Bug_9450" (919)
        self.phy
            .write(phy::STANDARD::MODE_CONTROL(), 0xC040.into())?;
        sleep_for(1); // line 934

        // We are now roughly at line 948, doing
        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //                  vtss_phy_conf_1g_set_private
        // Nothing to do here (automatic master/slave config is fine)

        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //                  vtss_phy_conf_set_private
        // Nothing happens in the first conditional (8581), because we're
        // in VTSS_PHY_MODE_FORCED and also doing passthrough mode (8739)

        // We are now at line 8968 and entering
        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //                  vtss_phy_conf_set_private
        //                      vtss_phy_pass_through_speed_mode (7601)
        //
        // However, even their SDK seems to _kinda_ rely on autonegotiation,
        // which we don't want: we control both ends of the system.  This is
        // therefore edited somewhat from their code.

        // The SDK enables autonegotiation on line 7614, but we don't want it.
        self.phy.modify(phy::STANDARD::MODE_CONTROL(), |r| {
            r.set_auto_neg_ena(0);
        })?;

        // "Protocol Transfer mode Guide : Section 4.1.3" (7625)
        // We are trying to do forced speed protocol transfer mode, so this
        // is the correct block.
        self.phy
            .modify(phy::EXTENDED_3::MAC_SERDES_PCS_CONTROL(), |r| {
                r.set_force_adv_ability(1);

                // This isn't in the SDK, but _maybe_ helps SGMII come up when
                // autonegotiation is disabled?  Or at least, it's used to that
                // effect elsewhere in the SDK.
                r.set_mac_if_pd_ena(1);
            })?;
        self.phy.write(
            phy::EXTENDED_3::MAC_SERDES_CLAUSE_37_ADVERTISED_ABILITY(),
            // VTSS_SPEED_100M (line 7630)
            0x8401.into(),
        )?;

        // Leave autonegotiation disabled (the SDK enables it on line 7659)

        // Configure SIGDET polarity on the media side; our SIGDET lines are
        // always pulled down.
        self.phy
            .modify(phy::EXTENDED::EXTENDED_MODE_CONTROL(), |r| {
                let mut r_ = u16::from(*r);
                r_ |= 1; // sigdet polarity = active low
                *r = r_.into();
            })?;

        // We are now done with vtss_phy_pass_through_speed_mode.
        // The rest of vtss_phy_conf_set_private doesn't do much (sets up fast
        // link fail enable), so we'll skip it for now.

        // Now, we're done with vtss_phy_soft_reset_port.
        // We are now roughly at line 980, doing
        //      phy_reset_private
        //          port_reset
        //              vtss_atom_patch_suspend(..., false)
        // and then returning from port_reset back into phy_reset_private
        crate::atom::atom_patch_resume(&mut self.phy)?;

        // There are no significant startup scripts for TESLA
        // (vtss_phy_100BaseT_long_linkup_workaround doesn't seem relevant,
        // called at line 8499)

        // We're now done with configuration from the SDK!

        // Enable the link state change mask, to detect PHY link flapping
        self.phy
            .modify(phy::STANDARD::INTERRUPT_MASK(), |r| r.set_link_mask(1))?;
        Ok(())
    }

    /// Configure the VSC8504 in MAC-to-Cat5 mode
    fn init_qsgmii_to_cat5(&mut self) -> Result<(), VscError> {
        ringbuf_entry!(Trace::Vsc8504Init(self.phy.port));

        let id = self.phy.read_id()?;
        if id != VSC8504_ID {
            return Err(VscError::BadPhyId(id));
        }

        let rev = self.phy.read(phy::GPIO::EXTENDED_REVISION())?;
        if rev.tesla_e() != 1 {
            return Err(VscError::BadPhyRev);
        }

        let phy_port = self.phy.get_port()?;
        let is_base_port = phy_port == 0;

        // `vtss_phy_pre_init_seq_tesla`, which calls
        // `vtss_phy_pre_init_seq_tesla_rev_e` (since we check above for rev E)
        if is_base_port {
            crate::tesla::TeslaPhy { phy: &mut self.phy }.patch()?;
        }

        // This is a TESLA PHY.  Here, we roughly follow the SDK function
        // `vtss_phy_reset_private`.  Please forgive the overlap with
        // vsc8562.rs; I'm doing my best to split into PHY-specific functions
        // instead of having MEGA-FUNCTIONS to handle every single PHY.

        // "-- Step 2: Pre-reset setup of MAC and Media interface --"
        // (There is no Step 1, apparently)
        //
        // We are now entering
        //      phy_reset_private
        //          vtss_phy_mac_media_if_tesla_setup

        // "Setup MAC Configuration" (5760)
        // (this is a global register, so we only need to write to it once, but
        // the SDK writes to it for each PHY, so we'll do the same)
        self.phy.modify(phy::GPIO::MAC_MODE_AND_FAST_LINK(), |r| {
            r.0 = (r.0 & !0x6000) | 0x4000
        })?;

        // "Configure SerDes macros for QSGMII MAC interface (See TN1080)"
        // (line 5836)
        if is_base_port {
            self.phy.cmd(0x80E0)?;
        }
        sleep_for(10); // (line 5928)

        // We are running with copper media, so
        // "Setup media in micro program" (5931)
        // "Turn off SerDes for 100Base-FX" (5934)
        self.phy.cmd(0x80F1 | (0x0100 << phy_port))?;
        // "Turn off SerDes for 1000Base-FX" (5934)
        self.phy.cmd(0x80E1 | (0x0100 << phy_port))?;
        sleep_for(10);

        // "Setup Media interface" (5952)
        self.phy
            .modify(phy::STANDARD::EXTENDED_PHY_CONTROL(), |r| {
                r.set_media_operating_mode(0b0); // Cat5 copper only
                r.set_ams_preference(1); // Cat5 copper preferred
            })?;

        // "Set packet mode" (line 5961)
        // Skipping this for now.

        // Congratulations!
        // You are now exiting vtss_phy_mac_media_if_tesla_setup and returning
        // to phy_reset_private:8313

        // "-- Step 3: Reset PHY --" (line 8349)
        // We are now entering
        //      phy_reset_private
        //          port_reset
        //              vtss_atom_patch_suspend(..., true)
        crate::atom::atom_patch_suspend(&mut self.phy)?;

        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //
        // The soft reset for the TESLA PHY is different, for some reason!
        // "Tesla PHY Only - Writing 0xc040, See Bug_9450" (919)
        self.phy
            .write(phy::STANDARD::MODE_CONTROL(), 0xC040.into())?;
        sleep_for(1); // line 934
                      // XXX the SDK loops here waiting for a flag to be clear

        // We are now roughly at line 948, doing
        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //                  vtss_phy_conf_1g_set_private
        // Nothing to do here (automatic master/slave config is fine)

        //      phy_reset_private
        //          port_reset
        //              vtss_phy_soft_reset_port
        //                  vtss_phy_conf_set_private
        // Leave auto-MDIX enabled by default (vtss_phy_mdi_setup)

        // We're now at line 8977, "Setup Reg16E3"
        // (this is copied from VSC8562 bringup, where it is sufficient to
        // connect to the tech ports; the actual vtss_phy_conf_set_private
        // includes a bunch more code, but who knows)
        self.phy
            .modify(phy::EXTENDED_3::MAC_SERDES_PCS_CONTROL(), |r| {
                // This should allow link-up if the MAC is not doing auto-neg.
                // Enable "MAC interface autonegotiation parallel detect",
                //    else data flow is stopped for the CU ports if PHY has MAC ANEG enabled and the switch is connected to isn't"
                r.set_mac_if_pd_ena(1);
            })?;
        // "Setup Reg23E3" (line 9002)
        // Nothing to do here, since we're not using the media SERDES?

        // Now, we're done with vtss_phy_soft_reset_port.
        // We are now roughly at line 980, doing
        //      phy_reset_private
        //          port_reset
        //              vtss_atom_patch_suspend(..., false)
        // and then returning from port_reset back into phy_reset_private
        crate::atom::atom_patch_resume(&mut self.phy)?;

        // There are no significant startup scripts for TESLA
        // (vtss_phy_100BaseT_long_linkup_workaround doesn't seem relevant,
        // called at line 8499)

        // We're now done with configuration from the SDK!

        // Enable the link state change mask, to detect PHY link flapping
        self.phy
            .modify(phy::STANDARD::INTERRUPT_MASK(), |r| r.set_link_mask(1))?;
        Ok(())
    }
}
