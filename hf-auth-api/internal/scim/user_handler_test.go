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
	created *UserAttributes
	updated *UserAttributes
	id      string
	err     error
}

func (m *mockUserService) CreateUser(_ context.Context, attributes *UserAttributes) (*User, error) {
	m.created = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &User{ID: "user-1", UserName: attributes.Username, Active: attributes.Active}, nil
}

func (m *mockUserService) UpdateUser(_ context.Context, id string, attributes *UserAttributes) (*User, error) {
	m.id = id
	m.updated = attributes
	if m.err != nil {
		return nil, m.err
	}
	return &User{ID: id, UserName: attributes.Username, Active: attributes.Active}, nil
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

func assertMappedAttributes(t *testing.T, attributes *UserAttributes) {
	t.Helper()
	if attributes == nil {
		t.Fatal("user service was not called")
	}
	if attributes.ExternalID != "external-7" ||
		attributes.Username != "ada" ||
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
