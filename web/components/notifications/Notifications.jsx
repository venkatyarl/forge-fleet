import React, { useState } from 'react';

const Notifications = () => {
    const [notifications, setNotifications] = useState([
        { id: 1, message: 'New task assigned!', type: 'urgent' },
        { id: 2, message: 'Meeting scheduled in 30 mins', type: 'normal' },
        { id: 3, message: 'Code review requested', type: 'general' }
    ]);

    const dismissNotification = (id) => {
        setNotifications(notifications.filter(notification => notification.id !== id));
    };

    return (
        <div className="notifications">
            <div className="top-bar">
                {notifications.map(notification => (
                    <div key={notification.id} className="notification">
                        <span className="message">{notification.message}</span>
                        <span className="type">{notification.type}</span>
                        <button onClick={() => dismissNotification(notification.id)}>Dismiss</button>
                    </div>
                ))}
            </div>
            <div className="right-rail">
                {notifications.map(notification => (
                    <div key={notification.id} className="notification">
                        <span className="message">{notification.message}</span>
                        <span className="type">{notification.type}</span>
                        <button onClick={() => dismissNotification(notification.id)}>Dismiss</button>
                    </div>
                ))}
            </div>
        </div>
    );
};

export default Notifications;
