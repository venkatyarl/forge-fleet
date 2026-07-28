package com.hireflow360.api.opportunities;

import com.hireflow360.api.opportunities.OpportunityDtos.CreateListingBody;
import com.hireflow360.api.opportunities.OpportunityDtos.UpdateListingBody;
import com.hireflow360.api.opportunities.OpportunityDtos.UpdateStageBody;
import com.hireflow360.api.security.CurrentTenant;
import com.hireflow360.api.security.Roles;
import com.hireflow360.api.security.TenantContext;
import jakarta.validation.Valid;
import org.springframework.http.HttpStatus;
import org.springframework.security.access.prepost.PreAuthorize;
import org.springframework.web.bind.annotation.*;

import java.util.List;
import java.util.Map;
import java.util.UUID;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Protected opportunities endpoints — FIRST BATCH (listings + applications).
 *
 * <p>Authorization policy (parity with the Rust handlers' doc comments):
 * <ul>
 *   <li>list/get listing: any authenticated org user (employees can browse).</li>
 *   <li>create/update/delete listing: recruiter-level ({@link Roles#RECRUITER}).</li>
 *   <li>list applications (applicant PII) + update stage: recruiter-level.</li>
 * </ul>
 * The two-layer security has already run before any method here:
 * {@code TenantAuthenticationFilter} resolved the tenant and
 * {@code EntitlementInterceptor} confirmed the {@code opportunities} subscription.
 */
@RestController
@RequestMapping("/opportunities")
public class OpportunityController {

    private static final Logger log = LoggerFactory.getLogger(OpportunityController.class);

    private final OpportunityService service;

    public OpportunityController(OpportunityService service) {
        this.service = service;
    }

    @GetMapping("/listings")
    public List<Listing> listListings(@RequestParam(name = "type", required = false) String type,
                                      @RequestParam(name = "status", required = false) String status,
                                      @RequestParam(name = "q", required = false) String q) {
        TenantContext t = CurrentTenant.require();
        return service.listListings(t.organizationId(), type, status, q);
    }

    /**
     * Lightweight typeahead for the global search bar (HFPROD-74). Tenant-scoped,
     * title-matched, and capped — any authenticated org user may query it.
     */
    @GetMapping("/listings/suggest")
    public List<OpportunityDtos.ListingSuggestion> suggestListings(
            @RequestParam(name = "q", required = false) String q,
            @RequestParam(name = "limit", required = false) Integer limit) {
        TenantContext t = CurrentTenant.require();
        return service.suggestListings(t.organizationId(), q, limit);
    }

    @PostMapping("/listings")
    @PreAuthorize(Roles.RECRUITER)
    @ResponseStatus(HttpStatus.CREATED)
    public Listing createListing(@Valid @RequestBody CreateListingBody body) {
        log.info("createListing request");
        TenantContext t = CurrentTenant.require();
        try {
            return service.createListing(t.organizationId(), body);
        } catch (Exception e) {
            log.error("listing_save_failed operation=createListing listingId=null orgId={} status={} type={} exception={}: {}",
                    t.organizationId(), body.status(), body.type(),
                    e.getClass().getSimpleName(), e.getMessage(), e);
            throw e;
        }
    }

    @GetMapping("/listings/{id}")
    public Listing getListing(@PathVariable UUID id) {
        TenantContext t = CurrentTenant.require();
        return service.getListing(id, t.organizationId());
    }

    @PatchMapping("/listings/{id}")
    @PreAuthorize(Roles.RECRUITER)
    public Listing updateListing(@PathVariable UUID id, @Valid @RequestBody UpdateListingBody body) {
        log.info("updateListing request");
        TenantContext t = CurrentTenant.require();
        try {
            return service.updateListing(id, t.organizationId(), body);
        } catch (Exception e) {
            log.error("listing_save_failed operation=updateListing listingId={} orgId={} status={} type={} exception={}: {}",
                    id, t.organizationId(), body.status(), body.type(),
                    e.getClass().getSimpleName(), e.getMessage(), e);
            throw e;
        }
    }

    @DeleteMapping("/listings/{id}")
    @PreAuthorize(Roles.RECRUITER)
    @ResponseStatus(HttpStatus.NO_CONTENT)
    public void deleteListing(@PathVariable UUID id) {
        log.info("deleteListing request");
        TenantContext t = CurrentTenant.require();
        service.deleteListing(id, t.organizationId());
    }

    @GetMapping("/listings/{listingId}/applications")
    @PreAuthorize(Roles.RECRUITER)
    public List<Application> listApplications(@PathVariable UUID listingId) {
        TenantContext t = CurrentTenant.require();
        return service.listApplications(listingId, t.organizationId());
    }

    @GetMapping("/listings/{id}/applicant-documents")
    @PreAuthorize(Roles.RECRUITER)
    public Map<String, Object> applicantDocuments(@PathVariable UUID id) {
        TenantContext t = CurrentTenant.require();
        return service.applicantDocuments(id, t.organizationId());
    }

    @PatchMapping("/applications/{id}/stage")
    @PreAuthorize(Roles.RECRUITER)
    public Application updateStage(@PathVariable UUID id, @Valid @RequestBody UpdateStageBody body) {
        log.info("updateStage request");
        TenantContext t = CurrentTenant.require();
        return service.updateStage(id, t.organizationId(), body.stage());
    }
}
