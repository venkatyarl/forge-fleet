package scim

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
)

var ErrMissingGroupDisplayName = errors.New("scim: displayName is required")

type Group struct {
	ID          string        `json:"id"`
	ExternalID  string        `json:"externalId,omitempty"`
	DisplayName string        `json:"displayName"`
	Members     []GroupMember `json:"members,omitempty"`
}

type GroupMember struct {
	Value   string `json:"value"`
	Display string `json:"display,omitempty"`
	Ref     string `json:"$ref,omitempty"`
	Type    string `json:"type,omitempty"`
}

type ScimGroupToken struct {
	ExternalID  string        `json:"externalId,omitempty"`
	DisplayName string        `json:"displayName"`
	Members     []GroupMember `json:"members,omitempty"`
}

type GroupService interface {
	CreateGroup(context.Context, *Group) (*Group, error)
	UpdateGroup(context.Context, string, *Group) (*Group, error)
}

type GroupHandler struct {
	groups GroupService
}

func NewGroupHandler(groups GroupService) *GroupHandler {
	return &GroupHandler{groups: groups}
}

func (h *GroupHandler) HandleCreateGroup(w http.ResponseWriter, r *http.Request) {
	attributes, ok := groupAttributesFromRequest(w, r)
	if !ok {
		return
	}
	group, err := h.groups.CreateGroup(r.Context(), attributes)
	if err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusCreated, group)
}

func (h *GroupHandler) HandleUpdateGroup(w http.ResponseWriter, r *http.Request) {
	attributes, ok := groupAttributesFromRequest(w, r)
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
		writeSCIMError(w, http.StatusBadRequest, "scim: group id is required")
		return
	}
	group, err := h.groups.UpdateGroup(r.Context(), id, attributes)
	if err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, group)
}

func ParseScimGroupToken(raw []byte) (*ScimGroupToken, error) {
	var token ScimGroupToken
	if err := json.Unmarshal(raw, &token); err != nil {
		return nil, fmt.Errorf("scim: decode group: %w", err)
	}
	if err := token.Validate(); err != nil {
		return nil, err
	}
	return &token, nil
}

func (t *ScimGroupToken) Validate() error {
	if t == nil || strings.TrimSpace(t.DisplayName) == "" {
		return ErrMissingGroupDisplayName
	}
	for _, member := range t.Members {
		if strings.TrimSpace(member.Value) == "" {
			return errors.New("scim: member value is required")
		}
	}
	return nil
}

func (t *ScimGroupToken) MapToGroupAttributes() (*Group, error) {
	if err := t.Validate(); err != nil {
		return nil, err
	}
	members := make([]GroupMember, 0, len(t.Members))
	for _, member := range t.Members {
		members = append(members, GroupMember{
			Value:   strings.TrimSpace(member.Value),
			Display: strings.TrimSpace(member.Display),
			Ref:     strings.TrimSpace(member.Ref),
			Type:    strings.TrimSpace(member.Type),
		})
	}
	return &Group{
		ExternalID:  strings.TrimSpace(t.ExternalID),
		DisplayName: strings.TrimSpace(t.DisplayName),
		Members:     members,
	}, nil
}

func groupAttributesFromRequest(w http.ResponseWriter, r *http.Request) (*Group, bool) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, "scim: read request body")
		return nil, false
	}
	token, err := ParseScimGroupToken(body)
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, err.Error())
		return nil, false
	}
	attributes, err := token.MapToGroupAttributes()
	if err != nil {
		writeSCIMError(w, http.StatusBadRequest, err.Error())
		return nil, false
	}
	return attributes, true
}
