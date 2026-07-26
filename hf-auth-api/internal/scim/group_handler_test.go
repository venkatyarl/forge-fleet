package scim

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

type mockGroupService struct {
	created *Group
	updated *Group
	id      string
	err     error
}

func (m *mockGroupService) CreateGroup(_ context.Context, attributes *Group) (*Group, error) {
	m.created = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &Group{ID: "group-1", DisplayName: attributes.DisplayName, Members: attributes.Members}, nil
}

func (m *mockGroupService) UpdateGroup(_ context.Context, id string, attributes *Group) (*Group, error) {
	m.id = id
	m.updated = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &Group{ID: id, DisplayName: attributes.DisplayName, Members: attributes.Members}, nil
}

func TestHandleCreateGroupMapsMembers(t *testing.T) {
	service := &mockGroupService{}
	handler := NewGroupHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Groups", strings.NewReader(validGroupJSON))
	response := httptest.NewRecorder()
	handler.HandleCreateGroup(response, request)
	if response.Code != http.StatusCreated {
		t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusCreated, response.Body)
	}
	assertMappedGroupAttributes(t, service.created)
}

func TestHandleUpdateGroupMapsMembers(t *testing.T) {
	service := &mockGroupService{}
	handler := NewGroupHandler(service)
	request := httptest.NewRequest(http.MethodPut, "/Groups/group-42", strings.NewReader(validGroupJSON))
	request.SetPathValue("id", "group-42")
	response := httptest.NewRecorder()
	handler.HandleUpdateGroup(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusOK, response.Body)
	}
	if service.id != "group-42" {
		t.Errorf("id = %q, want group-42", service.id)
	}
	assertMappedGroupAttributes(t, service.updated)
}

func TestHandleCreateGroupRejectsInvalidSCIM(t *testing.T) {
	service := &mockGroupService{}
	handler := NewGroupHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Groups", strings.NewReader(`{"members":[]}`))
	response := httptest.NewRecorder()
	handler.HandleCreateGroup(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusBadRequest)
	}
	if service.created != nil {
		t.Fatal("group service called for invalid SCIM input")
	}
}

func TestHandleCreateGroupRejectsMemberWithoutValue(t *testing.T) {
	service := &mockGroupService{}
	handler := NewGroupHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Groups", strings.NewReader(`{"displayName":"Engineering","members":[{"display":"Ada"}]}`))
	response := httptest.NewRecorder()
	handler.HandleCreateGroup(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusBadRequest)
	}
	if service.created != nil {
		t.Fatal("group service called for invalid SCIM input")
	}
}

func TestHandleUpdateGroupPropagatesServiceFailure(t *testing.T) {
	service := &mockGroupService{err: errors.New("update failed")}
	handler := NewGroupHandler(service)
	request := httptest.NewRequest(http.MethodPut, "/Groups/group-42", strings.NewReader(validGroupJSON))
	response := httptest.NewRecorder()
	handler.HandleUpdateGroup(response, request)
	if response.Code != http.StatusInternalServerError {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusInternalServerError)
	}
}

func assertMappedGroupAttributes(t *testing.T, attributes *Group) {
	t.Helper()
	if attributes == nil {
		t.Fatal("group service was not called")
	}
	if attributes.ExternalID != "external-group-7" || attributes.DisplayName != "Engineering" {
		t.Errorf("unexpected mapped group attributes: %+v", attributes)
	}
	if len(attributes.Members) != 2 {
		t.Fatalf("members length = %d, want 2", len(attributes.Members))
	}
	if attributes.Members[0] != (GroupMember{
		Value:   "user-1",
		Display: "Ada Lovelace",
		Ref:     "../Users/user-1",
		Type:    "User",
	}) {
		t.Errorf("members[0] = %+v", attributes.Members[0])
	}
	if attributes.Members[1] != (GroupMember{
		Value:   "user-2",
		Display: "Grace Hopper",
		Ref:     "../Users/user-2",
		Type:    "User",
	}) {
		t.Errorf("members[1] = %+v", attributes.Members[1])
	}
}

const validGroupJSON = `{
	"externalId":"external-group-7",
	"displayName":"Engineering",
	"members":[
		{"value":"user-1","display":"Ada Lovelace","$ref":"../Users/user-1","type":"User"},
		{"value":"user-2","display":"Grace Hopper","$ref":"../Users/user-2","type":"User"}
	]
}`
