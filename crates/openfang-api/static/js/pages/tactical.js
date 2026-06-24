// OpenFang Tactical Console — fast-loop situation + slow-loop cognitive planning
// with one-click intent injection, mission-plan approval, target authorization,
// and weapon-engagement signing.
'use strict';

function tacticalPage() {
  return {
    // ---- live data (from GET /api/platform/pending) ----
    control: null,
    planningEnabled: false,
    snapshot: null,
    report: null,
    assessmentThreats: [],
    assessmentOpportunities: [],
    intentQueue: [],
    labelResolutions: [],
    dslMissions: [],
    targetAuthorizations: [],
    missionApprovals: [],
    engagements: [],
    pendingIntentsCount: 0,

    // ---- M3-U5: autonomy profile + cerebellum service health ----
    autonomy: { activeId: '', envelope: null, profiles: [], overriddenAtRuntime: false },
    serviceHealth: { services: [], polled: false },
    autonomySwitching: false,

    // ---- M4-U6: fleet federation status + simulated link quality ----
    federation: {
      status: null,
      priorityOrder: [],
      staleCommandWindowS: 15,
      degradedProfileConfigured: null
    },
    federationLinkChoices: ['excellent', 'good', 'marginal', 'poor', 'lost'],
    federationLinkUpdating: false,

    // ---- DSL preview ----
    preview: null,
    previewing: false,

    loading: true,
    loadError: '',
    refreshTimer: null,

    // ---- intent injection form ----
    form: {
      objective: '',
      priority_tracks: '',
      priority_labels: '',
      issued_by: 'operator',
      constraints: ''
    },
    submitting: false,

    async loadData() {
      this.loadError = '';
      try {
        var d = await OpenFangAPI.get('/api/platform/pending');
        this.planningEnabled = !!d.planning_enabled;
        this.control = d.control || null;
        this.snapshot = d.snapshot || null;
        this.report = d.cognitive_report || null;
        var assessment = this.report && this.report.assessment;
        this.assessmentThreats = this.asArray(assessment && assessment.threats);
        this.assessmentOpportunities = this.asArray(assessment && assessment.opportunities);
        this.intentQueue = d.intent_queue || [];
        this.labelResolutions = d.label_resolutions || [];
        this.dslMissions = d.dsl_missions || [];
        this.targetAuthorizations = d.target_authorizations || [];
        this.missionApprovals = d.mission_approvals || [];
        this.engagements = d.engagements || [];
        this.pendingIntentsCount = d.pending_intents_count || 0;
      } catch (e) {
        this.loadError = (e && e.message) || 'Could not load tactical state. Is the platform loop enabled?';
      }
      // M3-U5: refresh autonomy envelope + service health in parallel. These
      // endpoints are independent of /api/platform/pending so we tolerate
      // failures (display a dim placeholder rather than blocking the page).
      try {
        var a = await OpenFangAPI.get('/api/autonomy/profile');
        this.autonomy = {
          activeId: a.active_id || '',
          envelope: a.active_envelope || null,
          profiles: a.profiles || [],
          overriddenAtRuntime: !!a.overridden_at_runtime
        };
      } catch (_) { /* fall back to last-known values */ }
      try {
        var h = await OpenFangAPI.get('/api/services/health');
        this.serviceHealth = {
          services: h.services || [],
          polled: !!h.polled
        };
      } catch (_) { /* fall back to last-known values */ }
      // M4-U6: federation status (leader/failover + degradation matrix).
      try {
        var f = await OpenFangAPI.get('/api/federation/status');
        this.federation = {
          status: f.status || null,
          priorityOrder: f.priority_order || [],
          staleCommandWindowS: typeof f.stale_command_window_s === 'number'
            ? f.stale_command_window_s : 15,
          degradedProfileConfigured: f.degraded_profile_configured || null
        };
      } catch (_) { /* fall back to last-known values */ }
      this.loading = false;
    },

    async setFederationLinkQuality(linkQuality) {
      if (!linkQuality) return;
      var status = this.federation && this.federation.status;
      if (status && status.link_quality === linkQuality) return;
      this.federationLinkUpdating = true;
      try {
        await OpenFangAPI.put('/api/federation/link_quality', {
          link_quality: linkQuality,
          actor: (this.form.issued_by || '').trim() || 'dashboard'
        });
        OpenFangToast.success('Simulated link quality → ' + linkQuality);
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to set link quality');
      }
      this.federationLinkUpdating = false;
    },

    async switchAutonomyProfile(id) {
      if (!id || id === this.autonomy.activeId) return;
      this.autonomySwitching = true;
      try {
        await OpenFangAPI.put('/api/autonomy/profile', {
          id: id,
          actor: this.form.issued_by.trim() || 'dashboard'
        });
        OpenFangToast.success('Autonomy profile switched to ' + id);
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to switch autonomy profile');
      }
      this.autonomySwitching = false;
    },

    startAutoRefresh() {
      this.stopAutoRefresh();
      var self = this;
      this.refreshTimer = setInterval(function () { self.loadData(); }, 3000);
    },

    stopAutoRefresh() {
      if (this.refreshTimer) {
        clearInterval(this.refreshTimer);
        this.refreshTimer = null;
      }
    },

    // ---- derived helpers ----
    platformSideMatches(affiliation) {
      var side = (this.control && this.control.controlled_side) || 'blue';
      if (side === 'blue_and_friend') {
        return affiliation === 'blue' || affiliation === 'friend';
      }
      return affiliation === side;
    },

    get ownPlatforms() {
      if (!this.snapshot || !this.snapshot.platforms) return [];
      var allow = (this.control && this.control.controlled_platforms) || [];
      return this.snapshot.platforms.filter(function (p) {
        if (!this.platformSideMatches(p.affiliation)) return false;
        if (!allow.length) return true;
        return allow.indexOf(p.id) >= 0;
      }.bind(this));
    },

    get controlScopeLabel() {
      if (!this.control) return '—';
      var side = this.control.controlled_side || 'blue';
      var ids = this.control.controlled_platforms || [];
      if (!ids.length) return '控制方: ' + side + ' · 实体: 全部';
      return '控制方: ' + side + ' · 实体: ' + ids.join(', ');
    },

    get hostileTracks() {
      if (!this.snapshot || !this.snapshot.platforms) return [];
      var rows = [];
      this.snapshot.platforms.forEach(function (p) {
        (p.tracks || []).forEach(function (t) { rows.push(t); });
      });
      return rows;
    },

    get pendingPlanId() {
      return this.report && this.report.pending_approval_id ? this.report.pending_approval_id : '';
    },

    asArray(value) {
      if (Array.isArray(value)) return value;
      if (value && typeof value === 'object') return Object.values(value);
      return [];
    },

    pct(v) {
      if (v === null || v === undefined) return '—';
      return (v * 100).toFixed(0) + '%';
    },

    num(v, digits) {
      if (v === null || v === undefined) return '—';
      return Number(v).toFixed(digits === undefined ? 0 : digits);
    },

    // ---- actions ----
    async submitIntent() {
      if (!this.form.objective.trim()) {
        OpenFangToast.error('Objective is required');
        return;
      }
      this.submitting = true;
      var now = Date.now() / 1000;
      var tracks = this.form.priority_tracks
        .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
      var labels = this.form.priority_labels
        .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
      var constraints = this.form.constraints
        .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
      var intent = {
        id: 'intent-' + Date.now(),
        issued_at: now,
        issued_by: this.form.issued_by.trim()
          || (this.control && this.control.controller_id)
          || 'operator',
        objective: this.form.objective.trim(),
        priority_tracks: tracks,
        priority_labels: labels,
        constraints: constraints,
        roe_pref: null
      };
      try {
        await OpenFangAPI.post('/api/platform/intent', intent);
        OpenFangToast.success('Intent injected');
        this.form.objective = '';
        this.form.priority_tracks = '';
        this.form.priority_labels = '';
        this.form.constraints = '';
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to inject intent');
      }
      this.submitting = false;
    },

    // ---- DSL mission pipeline ----
    async previewIntent() {
      if (!this.form.objective.trim()) {
        OpenFangToast.error('Objective is required');
        return;
      }
      this.previewing = true;
      this.preview = null;
      try {
        var tracks = this.form.priority_tracks
          .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
        var labels = this.form.priority_labels
          .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
        var constraints = this.form.constraints
          .split(',').map(function (s) { return s.trim(); }).filter(Boolean);
        this.preview = await OpenFangAPI.post('/api/platform/intent/preview', {
          objective: this.form.objective.trim(),
          priority_tracks: tracks,
          priority_labels: labels,
          constraints: constraints,
          roe_pref: null
        });
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to compile DSL preview');
      }
      this.previewing = false;
    },

    clearPreview() {
      this.preview = null;
    },

    async confirmMission(id) {
      if (!id) return;
      try {
        var r = await OpenFangAPI.post('/api/platform/missions/' + id + '/confirm', {
          confirmed_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Mission dispatched (' + (r.emitted_intents || 0) + ' intents)');
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to confirm mission');
      }
    },

    async dismissMission(id) {
      if (!id) return;
      try {
        await OpenFangAPI.post('/api/platform/missions/' + id + '/dismiss', {
          dismissed_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Mission dismissed');
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to dismiss mission');
      }
    },

    candidateSummary(candidate) {
      if (!candidate) return '';
      var parts = [candidate.track_id];
      if (candidate.source_platform_name) parts.push(candidate.source_platform_name);
      if (candidate.platform_type) parts.push(candidate.platform_type);
      if (candidate.track_classification) parts.push(candidate.track_classification);
      parts.push('score ' + this.num(candidate.score, 1));
      if (candidate.weapon_reachable) parts.push('weapon reachable');
      return parts.join(' · ');
    },

    async confirmLabelResolution(id) {
      if (!id) return;
      try {
        await OpenFangAPI.post('/api/platform/label-resolutions/' + id + '/confirm', {
          confirmed_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Label resolution confirmed');
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to confirm label resolution');
      }
    },

    async dismissLabelResolution(id) {
      if (!id) return;
      try {
        await OpenFangAPI.post('/api/platform/label-resolutions/' + id + '/dismiss', {
          dismissed_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Label resolution dismissed');
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to dismiss label resolution');
      }
    },

    async approvePlan(planId) {
      if (!planId) return;
      try {
        await OpenFangAPI.post('/api/platform/missions/approve', {
          plan_id: planId,
          approved_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Plan approved — releases next cycle');
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to approve plan');
      }
    },

    async authorizeTarget(platformId, trackId) {
      if (!platformId || !trackId) {
        OpenFangToast.error('platform_id and track_id required');
        return;
      }
      try {
        await OpenFangAPI.post('/api/platform/targets/authorize', {
          platform_id: platformId,
          track_id: trackId,
          authorized_by: this.form.issued_by.trim() || 'operator'
        });
        OpenFangToast.success('Target authorized: ' + platformId + ' → ' + trackId);
        await this.loadData();
      } catch (e) {
        OpenFangToast.error((e && e.message) || 'Failed to authorize target');
      }
    },

    async signEngagement(id) {
      if (!id) return;
      var self = this;
      OpenFangToast.confirm('Sign & Launch', 'Sign and launch engagement ' + id + '?', async function () {
        try {
          await OpenFangAPI.post('/api/platform/engagements/' + id + '/sign', {
            signer: self.form.issued_by.trim() || 'operator',
            launch_if_ready: true
          });
          OpenFangToast.success('Engagement signed: ' + id);
          await self.loadData();
        } catch (e) {
          OpenFangToast.error((e && e.message) || 'Failed to sign engagement');
        }
      });
    },

    canSignEngagement(e) {
      if (!e || !e.state) return false;
      return e.state === 'pending_signatures' || e.state === 'approved';
    },

    engagementTerminalLabel(e) {
      if (!e || !e.state) return '';
      if (e.state === 'launched') return 'launched';
      if (e.state === 'aborted') return 'launch failed/aborted';
      if (e.state === 'expired') return 'expired';
      if (e.state === 'rejected') return 'rejected';
      if (e.state === 'armed') return 'armed';
      return 'tracked';
    }
  };
}
