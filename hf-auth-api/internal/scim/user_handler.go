package scim

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strconv"
	"strings"
)

const scimErrorSchema = "urn:ietf:params:scim:api:messages:2.0:Error"

type User struct {
	ID          string `json:"id"`
	ExternalID  string `json:"externalId,omitempty"`
	UserName    string `json:"userName"`
	Email       string `json:"email,omitempty"`
	DisplayName string `json:"displayName,omitempty"`
	Active      bool   `json:"active"`
}

type UserService interface {
	CreateUser(context.Context, *User) (*User, error)
	UpdateUser(context.Context, string, *User) (*User, error)
}

type UserHandler struct {
	users UserService
}

func NewUserHandler(users UserService) *UserHandler {
	return &UserHandler{users: users}
}

func (h *UserHandler) HandleCreateUser(w http.ResponseWriter, r *http.Request) {
	attributes, ok := attributesFromRequest(w, r)
	if !ok {
		return
	}
	user, err := h.users.CreateUser(r.Context(), attributes)
	if err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusCreated, user)
}

func (h *UserHandler) HandleUpdateUser(w http.ResponseWriter, r *http.Request) {
	attributes, ok := attributesFromRequest(w, r)
	if !ok {
		return
	}
	id := r.PathValue("id")
	if id == "" {
		id = strings.TrimSuffix(r.URL.Path, "/")
		if slash := strings.LastIndexByte(id, '/'); slash >= 0 {
			id = id[slash+1:]
		}
	}
	if id == "" {
		writeSCIMError(w, http.StatusBadRequest, "scim: user id is required")
		return
	}
	user, err := h.users.UpdateUser(r.Context(), id, attributes)
	if err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, user)
}

func attributesFromRequest(w http.ResponseWriter, r *http.Request) (*User, bool) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, "scim: read request body")
		return nil, false
	}
	token, err := ParseScimToken(body)
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, err.Error())
		return nil, false
	}
	attributes, err := token.MapToUserAttributes()
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, err.Error())
		return nil, false
	}
	return attributes, true
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/scim+json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}

func writeSCIMError(w http.ResponseWriter, status int, detail string) {
	writeJSON(w, status, struct {
		Schemas []string `json:"schemas"`
		Status  string   `json:"status"`
		Detail  string   `json:"detail"`
	}{
		Schemas: []string{scimErrorSchema},
		Status:  strconv.Itoa(status),
		Detail:  detail,
	})
}
