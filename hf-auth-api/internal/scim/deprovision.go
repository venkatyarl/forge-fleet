package scim

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"sync"
)

const UserDeprovisionedEventType = "UserDeprovisioned"

type UserDeprovisionedEvent struct {
	Type       string `json:"type"`
	UserID     string `json:"userId"`
	ExternalID string `json:"externalId,omitempty"`
	UserName   string `json:"userName,omitempty"`
}

type UserDeactivationService interface {
	DeactivateUser(context.Context, string) (*User, error)
}

type UserDeactivationOnlyService interface {
	DeactivateUser(context.Context, string) error
}

type UserDeactivateOnlyService interface {
	Deactivate(context.Context, string) error
}

type UserDeprovisionedPublisher interface {
	PublishUserDeprovisioned(context.Context, UserDeprovisionedEvent) error
}

type EventPublisher interface {
	Publish(context.Context, any) error
}

type NamedEventPublisher interface {
	Publish(context.Context, string, any) error
}

func (h *UserHandler) HandleDeleteUser(w http.ResponseWriter, r *http.Request) {
	id := userIDFromRequest(r)
	if id == "" {
		writeSCIMError(w, http.StatusBadRequest, "scim: user id is required")
		return
	}
	user, err := h.deactivateUser(r.Context(), id)
	if err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	if err := h.publishUserDeprovisioned(r.Context(), user); err != nil {
		writeSCIMError(w, http.StatusInternalServerError, err.Error())
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (h *UserHandler) deactivateUser(ctx context.Context, id string) (*User, error) {
	switch service := h.users.(type) {
	case UserDeactivationService:
		user, err := service.DeactivateUser(ctx, id)
		if err != nil {
			return nil, err
		}
		return inactiveUser(id, user), nil
	case UserDeactivationOnlyService:
		if err := service.DeactivateUser(ctx, id); err != nil {
			return nil, err
		}
		return inactiveUser(id, nil), nil
	case UserDeactivateOnlyService:
		if err := service.Deactivate(ctx, id); err != nil {
			return nil, err
		}
		return inactiveUser(id, nil), nil
	default:
		return nil, errors.New("scim: user deactivation service is not configured")
	}
}

func inactiveUser(id string, user *User) *User {
	if user == nil {
		user = &User{ID: id, Active: false}
	}
	user.ID = strings.TrimSpace(user.ID)
	if user.ID == "" {
		user.ID = id
	}
	user.Active = false
	return user
}

func (h *UserHandler) publishUserDeprovisioned(ctx context.Context, user *User) error {
	if h.events == nil || user == nil || strings.TrimSpace(user.ID) == "" {
		return nil
	}
	event := UserDeprovisionedEvent{
		Type:       UserDeprovisionedEventType,
		UserID:     strings.TrimSpace(user.ID),
		ExternalID: strings.TrimSpace(user.ExternalID),
		UserName:   strings.TrimSpace(user.UserName),
	}
	if markUserDeprovisionedEventEmitted(h, event.UserID) {
		return nil
	}
	if err := publishUserDeprovisioned(ctx, h.events, event); err != nil {
		unmarkUserDeprovisionedEventEmitted(h, event.UserID)
		return err
	}
	return nil
}

func publishUserDeprovisioned(ctx context.Context, events any, event UserDeprovisionedEvent) error {
	switch bus := events.(type) {
	case UserDeprovisionedPublisher:
		return bus.PublishUserDeprovisioned(ctx, event)
	case NamedEventPublisher:
		return bus.Publish(ctx, event.Type, event)
	case EventPublisher:
		return bus.Publish(ctx, event)
	default:
		return errors.New("scim: event bus does not support UserDeprovisioned")
	}
}

var userDeprovisionedEvents sync.Map

func markUserDeprovisionedEventEmitted(handler *UserHandler, userID string) bool {
	key := deprovisionedEventKey(handler, userID)
	_, loaded := userDeprovisionedEvents.LoadOrStore(key, struct{}{})
	return loaded
}

func unmarkUserDeprovisionedEventEmitted(handler *UserHandler, userID string) {
	userDeprovisionedEvents.Delete(deprovisionedEventKey(handler, userID))
}

func deprovisionedEventKey(handler *UserHandler, userID string) struct {
	handler *UserHandler
	userID  string
} {
	return struct {
		handler *UserHandler
		userID  string
	}{handler: handler, userID: userID}
}

func userIDFromRequest(r *http.Request) string {
	id := r.PathValue("id")
	if id == "" {
		id = strings.TrimSuffix(r.URL.Path, "/")
		if slash := strings.LastIndexByte(id, '/'); slash >= 0 {
			id = id[slash+1:]
		}
	}
	return strings.TrimSpace(id)
}
