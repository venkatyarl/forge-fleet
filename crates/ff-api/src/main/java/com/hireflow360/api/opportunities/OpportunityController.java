package com.hireflow360.api.opportunities;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/opportunities")
public class OpportunityController {
    private static final Logger logger = LoggerFactory.getLogger(OpportunityController.class);
    private final OpportunityService opportunityService;

    public OpportunityController(OpportunityService opportunityService) {
        this.opportunityService = opportunityService;
    }

    @PostMapping
    public ResponseEntity<?> createOpportunity(@RequestBody OpportunityRequest request) {
        try {
            Opportunity opportunity = opportunityService.saveOpportunity(request, request.getOrgId());
            return ResponseEntity.ok(opportunity);
        } catch (Exception e) {
            logger.error("Failed to create opportunity - orgId: {}, error: {}", 
                request.getOrgId(), e.getMessage(), e);
            throw e;
        }
    }

    @PutMapping("/{id}")
    public ResponseEntity<?> updateOpportunity(@PathVariable String id, @RequestBody OpportunityRequest request) {
        try {
            Opportunity opportunity = opportunityService.updateOpportunity(id, request, request.getOrgId());
            return ResponseEntity.ok(opportunity);
        } catch (Exception e) {
            logger.error("Failed to update opportunity - orgId: {}, listingId: {}, error: {}", 
                request.getOrgId(), id, e.getMessage(), e);
            throw e;
        }
    }

    @GetMapping("/{id}/documents")
    public ResponseEntity<?> applicantDocuments(@PathVariable String id) {
        try {
            return ResponseEntity.ok(opportunityService.getApplicantDocuments(id));
        } catch (Exception e) {
            logger.error("Failed to get applicant documents - listingId: {}, error: {}", 
                id, e.getMessage(), e);
            throw e;
        }
    }
}
