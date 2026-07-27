package scim

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

type mockUserService struct {
	created       *User
	updated       *User
	deactivated   string
	id            string
	err           error
	deactiveErr   error
	deactivations int
}

func (m *mockUserService) CreateUser(_ context.Context, attributes *User) (*User, error) {
	m.created = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &User{ID: "user-1", UserName: attributes.UserName, Active: attributes.Active}, nil
}

func (m *mockUserService) UpdateUser(_ context.Context, id string, attributes *User) (*User, error) {
	m.id = id
	m.updated = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &User{ID: id, UserName: attributes.UserName, Active: attributes.Active}, nil
}

func (m *mockUserService) DeactivateUser(_ context.Context, id string) (*User, error) {
	m.deactivated = id
	m.deactivations++
	if m.deactiveErr != nil {
		return nil, m.deactiveErr
	}
	return &User{ID: id, UserName: "ada", ExternalID: "external-7", Active: false}, nil
}

type mockEventBus struct {
	events []UserDeprovisionedEvent
	err    error
}

func (m *mockEventBus) PublishUserDeprovisioned(_ context.Context, event UserDeprovisionedEvent) error {
	if m.err != nil {
		return m.err
	}
	m.events = append(m.events, event)
	return nil
}

func TestHandleCreateUserMapsAttributes(t *testing.T) {
	service := &mockUserService{}
	handler := NewUserHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Users", strings.NewReader(validUserJSON))
	response := httptest.NewRecorder()
	handler.HandleCreateUser(response, request)
	if response.Code != http.StatusCreated {
		t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusCreated, response.Body)
	}
	assertMappedAttributes(t, service.created)
}

func TestHandleUpdateUserMapsAttributes(t *testing.T) {
	service := &mockUserService{}
	handler := NewUserHandler(service)
	request := httptest.NewRequest(http.MethodPut, "/Users/user-42", strings.NewReader(validUserJSON))
	request.SetPathValue("id", "user-42")
	response := httptest.NewRecorder()
	handler.HandleUpdateUser(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusOK, response.Body)
	}
	if service.id != "user-42" {
		t.Errorf("id = %q, want user-42", service.id)
	}
	assertMappedAttributes(t, service.updated)
}

func TestHandleUpdateUserInactiveDeactivatesAndPublishesOnce(t *testing.T) {
	service := &mockUserService{}
	events := &mockEventBus{}
	handler := NewUserHandlerWithEventBus(service, events)
	for i := 0; i < 2; i++ {
		request := httptest.NewRequest(http.MethodPut, "/Users/user-42", strings.NewReader(inactiveUserJSON))
		request.SetPathValue("id", "user-42")
		response := httptest.NewRecorder()
		handler.HandleUpdateUser(response, request)
		if response.Code != http.StatusOK {
			t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusOK, response.Body)
		}
	}
	if service.deactivations != 2 {
		t.Errorf("deactivations = %d, want 2", service.deactivations)
	}
	if service.updated != nil {
		t.Fatal("update service called for inactive SCIM user")
	}
	if len(events.events) != 1 {
		t.Fatalf("events = %d, want 1", len(events.events))
	}
	if events.events[0].Type != UserDeprovisionedEventType || events.events[0].UserID != "user-42" {
		t.Errorf("unexpected event: %+v", events.events[0])
	}
}

func TestHandleDeleteUserDeactivatesAndPublishesOnce(t *testing.T) {
	service := &mockUserService{}
	events := &mockEventBus{}
	handler := NewUserHandlerWithEventBus(service, events)
	for i := 0; i < 2; i++ {
		request := httptest.NewRequest(http.MethodDelete, "/Users/user-42", nil)
		request.SetPathValue("id", "user-42")
		response := httptest.NewRecorder()
		handler.HandleDeleteUser(response, request)
		if response.Code != http.StatusNoContent {
			t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusNoContent, response.Body)
		}
	}
	if service.deactivations != 2 {
		t.Errorf("deactivations = %d, want 2", service.deactivations)
	}
	if len(events.events) != 1 {
		t.Fatalf("events = %d, want 1", len(events.events))
	}
	if events.events[0].Type != UserDeprovisionedEventType || events.events[0].UserID != "user-42" {
		t.Errorf("unexpected event: %+v", events.events[0])
	}
}

func TestHandleCreateUserRejectsInvalidSCIM(t *testing.T) {
	service := &mockUserService{}
	handler := NewUserHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Users", strings.NewReader(`{"emails":[]}`))
	response := httptest.NewRecorder()
	handler.HandleCreateUser(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusBadRequest)
	}
	if service.created != nil {
		t.Fatal("user service called for invalid SCIM input")
	}
}

func TestHandleCreateUserAllowsMissingEmails(t *testing.T) {
	service := &mockUserService{}
	handler := NewUserHandler(service)
	request := httptest.NewRequest(http.MethodPost, "/Users", strings.NewReader(`{"userName":"ada","active":true}`))
	response := httptest.NewRecorder()
	handler.HandleCreateUser(response, request)
	if response.Code != http.StatusCreated {
		t.Fatalf("status = %d, want %d; body=%s", response.Code, http.StatusCreated, response.Body)
	}
	if service.created == nil {
		t.Fatal("user service was not called")
	}
	if service.created.Email != "" {
		t.Errorf("email = %q, want empty", service.created.Email)
	}
}

func TestHandleUpdateUserPropagatesServiceFailure(t *testing.T) {
	service := &mockUserService{err: errors.New("update failed")}
	handler := NewUserHandler(service)
	request := httptest.NewRequest(http.MethodPut, "/Users/user-42", strings.NewReader(validUserJSON))
	response := httptest.NewRecorder()
	handler.HandleUpdateUser(response, request)
	if response.Code != http.StatusInternalServerError {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusInternalServerError)
	}
}

func assertMappedAttributes(t *testing.T, attributes *User) {
	t.Helper()
	if attributes == nil {
		t.Fatal("user service was not called")
	}
	if attributes.ExternalID != "external-7" ||
		attributes.UserName != "ada" ||
		attributes.Email != "ada@example.com" ||
		attributes.DisplayName != "Ada Lovelace" ||
		!attributes.Active {
		t.Errorf("unexpected mapped attributes: %+v", attributes)
	}
}

const validUserJSON = `{
	"externalId":"external-7",
	"userName":"ada",
	"displayName":"Ada Lovelace",
	"emails":[
		{"value":"other@example.com"},
		{"value":"ada@example.com","primary":true}
	],
	"active":true
}`

const inactiveUserJSON = `{
	"externalId":"external-7",
	"userName":"ada",
	"emails":[{"value":"ada@example.com","primary":true}],
	"active":false
}`
